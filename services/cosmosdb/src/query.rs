//! The Cosmos DB for NoSQL query language: the subset applications use.
//!
//! `SELECT [DISTINCT] [TOP n] [VALUE] <projection> FROM <alias> [JOIN x IN
//! alias.path ...] [WHERE <expr>] [GROUP BY <expr>, ...] [ORDER BY <expr>
//! [ASC|DESC], ...] [OFFSET n LIMIT m]`, with `@parameters`; property paths
//! (`c.a.b`, `c["a"]`, `c.tags[0]`); object and array literals; arithmetic,
//! comparison, `AND`/`OR`/`NOT`, `IN`, `BETWEEN`, `LIKE`, `??` and `?:`;
//! aggregates `COUNT`, `SUM`, `MIN`, `MAX`, `AVG`; and the common built-in
//! functions. A missing property is *undefined*, as in Cosmos DB: a
//! comparison with it is undefined, `WHERE` keeps only `true`, and a
//! projection leaves it out.

use std::cmp::Ordering;

use serde_json::{json, Map, Number, Value};

/// A value or undefined (`None`).
type Val = Option<Value>;

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Ident(String),
    Str(String),
    Num(f64),
    Param(String),
    Op(&'static str),
}

fn tokenize(text: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && chars.get(i + 1) == Some(&'-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '\'' || c == '"' {
            let quote = c;
            let mut value = String::new();
            i += 1;
            loop {
                match chars.get(i) {
                    None => return Err("A string literal is not closed.".into()),
                    Some('\\') => {
                        let escaped = *chars.get(i + 1).ok_or("A string literal is not closed.")?;
                        value.push(match escaped {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            other => other,
                        });
                        i += 2;
                    }
                    Some(&q) if q == quote => {
                        i += 1;
                        break;
                    }
                    Some(&other) => {
                        value.push(other);
                        i += 1;
                    }
                }
            }
            tokens.push(Token::Str(value));
        } else if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit)) {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == 'e' || chars[i] == 'E'
                || ((chars[i] == '-' || chars[i] == '+') && matches!(chars[i - 1], 'e' | 'E')))
            {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            tokens.push(Token::Num(text.parse().map_err(|_| format!("'{text}' is not a number."))?));
        } else if c == '@' {
            let start = i;
            i += 1;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            tokens.push(Token::Param(chars[start..i].iter().collect()));
        } else if c.is_alphabetic() || c == '_' || c == '$' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                i += 1;
            }
            tokens.push(Token::Ident(chars[start..i].iter().collect()));
        } else {
            let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
            if let Some(op) = ["<>", "!=", "<=", ">=", "||", "??"].into_iter().find(|op| *op == two) {
                tokens.push(Token::Op(op));
                i += 2;
            } else {
                let op = ["=", "<", ">", "+", "-", "*", "/", "%", "(", ")", "[", "]", "{", "}", ",", ".", ":", "?"]
                    .into_iter()
                    .find(|op| op.starts_with(c))
                    .ok_or_else(|| format!("Syntax error, unexpected character '{c}'."))?;
                tokens.push(Token::Op(op));
                i += 1;
            }
        }
    }
    Ok(tokens)
}

#[derive(Clone, Debug)]
enum Expr {
    Literal(Value),
    Undefined,
    Param(String),
    /// A FROM or JOIN alias.
    Alias(String),
    Member(Box<Expr>, Box<Expr>),
    Object(Vec<(String, Expr)>),
    Array(Vec<Expr>),
    Unary(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    In(Box<Expr>, Vec<Expr>, bool),
    Between(Box<Expr>, Box<Expr>, Box<Expr>, bool),
    Like(Box<Expr>, Box<Expr>, bool),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
    Exists(Box<Query>),
}

#[derive(Clone, Debug)]
enum Projection {
    Star,
    Value(Expr),
    Fields(Vec<(Option<String>, Expr)>),
}

#[derive(Clone, Debug)]
struct Join {
    alias: String,
    source: Expr,
}

/// A parsed query.
#[derive(Clone, Debug)]
pub struct Query {
    distinct: bool,
    top: Option<Expr>,
    projection: Projection,
    alias: String,
    /// `FROM x IN c.path`: iterate a path of each document.
    from_in: Option<Expr>,
    joins: Vec<Join>,
    filter: Option<Expr>,
    group_by: Vec<Expr>,
    order_by: Vec<(Expr, bool)>,
    offset: Option<Expr>,
    limit: Option<Expr>,
}

const KEYWORDS: &[&str] = &[
    "select", "from", "where", "and", "or", "not", "in", "between", "like", "order", "by", "asc", "desc", "top", "value",
    "distinct", "as", "join", "offset", "limit", "group", "true", "false", "null", "undefined", "exists", "escape",
];

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at).cloned();
        self.at += 1;
        token
    }

    fn is_keyword(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Ident(w)) if w.eq_ignore_ascii_case(word))
    }

    fn eat_keyword(&mut self, word: &str) -> bool {
        let found = self.is_keyword(word);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect_keyword(&mut self, word: &str) -> Result<(), String> {
        if self.eat_keyword(word) {
            Ok(())
        } else {
            Err(format!("Syntax error, expected {}.", word.to_uppercase()))
        }
    }

    /// Whether `(SELECT` comes next: a subquery as a function's argument.
    fn subquery_follows(&self) -> bool {
        matches!(self.tokens.get(self.at), Some(Token::Op("(")))
            && matches!(self.tokens.get(self.at + 1), Some(Token::Ident(w)) if w.eq_ignore_ascii_case("select"))
    }

    fn eat_op(&mut self, op: &str) -> bool {
        let found = matches!(self.peek(), Some(Token::Op(o)) if *o == op);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect_op(&mut self, op: &str) -> Result<(), String> {
        if self.eat_op(op) {
            Ok(())
        } else {
            Err(format!("Syntax error, expected '{op}' near {:?}.", self.peek()))
        }
    }

    fn identifier(&mut self) -> Result<String, String> {
        match self.next() {
            Some(Token::Ident(name)) if !KEYWORDS.contains(&name.to_ascii_lowercase().as_str()) => Ok(name),
            other => Err(format!("Syntax error, expected an identifier, found {other:?}.")),
        }
    }

    fn query(&mut self) -> Result<Query, String> {
        self.expect_keyword("select")?;
        let distinct = self.eat_keyword("distinct");
        let top = if self.eat_keyword("top") { Some(self.primary()?) } else { None };
        let projection = if self.eat_op("*") {
            Projection::Star
        } else if self.eat_keyword("value") {
            Projection::Value(self.expression()?)
        } else {
            let mut fields = Vec::new();
            loop {
                let expr = self.expression()?;
                let named = self.eat_keyword("as")
                    || matches!(self.peek(), Some(Token::Ident(w)) if !KEYWORDS.contains(&w.to_ascii_lowercase().as_str()));
                let alias = if named { Some(self.identifier()?) } else { None };
                fields.push((alias, expr));
                if !self.eat_op(",") {
                    break;
                }
            }
            Projection::Fields(fields)
        };
        let (alias, from_in) = if self.eat_keyword("from") {
            let first = self.identifier()?;
            if self.eat_keyword("in") {
                (first, Some(self.expression()?))
            } else {
                let _ = self.eat_keyword("as");
                let alias = match self.peek() {
                    Some(Token::Ident(w)) if !KEYWORDS.contains(&w.to_ascii_lowercase().as_str()) => self.identifier()?,
                    _ => first,
                };
                (alias, None)
            }
        } else {
            ("$root".to_string(), None)
        };
        let mut joins = Vec::new();
        while self.eat_keyword("join") {
            let alias = self.identifier()?;
            self.expect_keyword("in")?;
            joins.push(Join { alias, source: self.expression()? });
        }
        let filter = if self.eat_keyword("where") { Some(self.expression()?) } else { None };
        let mut group_by = Vec::new();
        if self.eat_keyword("group") {
            self.expect_keyword("by")?;
            loop {
                group_by.push(self.expression()?);
                if !self.eat_op(",") {
                    break;
                }
            }
        }
        let mut order_by = Vec::new();
        if self.eat_keyword("order") {
            self.expect_keyword("by")?;
            loop {
                let expr = self.expression()?;
                let descending = if self.eat_keyword("desc") {
                    true
                } else {
                    let _ = self.eat_keyword("asc");
                    false
                };
                order_by.push((expr, descending));
                if !self.eat_op(",") {
                    break;
                }
            }
        }
        let (offset, limit) = if self.eat_keyword("offset") {
            let offset = self.primary()?;
            self.expect_keyword("limit")?;
            (Some(offset), Some(self.primary()?))
        } else {
            (None, None)
        };
        Ok(Query { distinct, top, projection, alias, from_in, joins, filter, group_by, order_by, offset, limit })
    }

    fn expression(&mut self) -> Result<Expr, String> {
        let condition = self.coalesce()?;
        if self.eat_op("?") {
            let then = self.expression()?;
            self.expect_op(":")?;
            let otherwise = self.expression()?;
            return Ok(Expr::Ternary(Box::new(condition), Box::new(then), Box::new(otherwise)));
        }
        Ok(condition)
    }

    fn coalesce(&mut self) -> Result<Expr, String> {
        let mut left = self.or()?;
        while self.eat_op("??") {
            left = Expr::Binary("??", Box::new(left), Box::new(self.or()?));
        }
        Ok(left)
    }

    fn or(&mut self) -> Result<Expr, String> {
        let mut left = self.and()?;
        while self.eat_keyword("or") {
            left = Expr::Binary("or", Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, String> {
        let mut left = self.not()?;
        while self.eat_keyword("and") {
            left = Expr::Binary("and", Box::new(left), Box::new(self.not()?));
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Expr, String> {
        if self.eat_keyword("not") {
            return Ok(Expr::Unary("not", Box::new(self.not()?)));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Expr, String> {
        let left = self.additive()?;
        let negated = self.eat_keyword("not");
        if self.eat_keyword("in") {
            self.expect_op("(")?;
            let mut items = vec![self.expression()?];
            while self.eat_op(",") {
                items.push(self.expression()?);
            }
            self.expect_op(")")?;
            return Ok(Expr::In(Box::new(left), items, negated));
        }
        if self.eat_keyword("between") {
            let low = self.additive()?;
            self.expect_keyword("and")?;
            let high = self.additive()?;
            return Ok(Expr::Between(Box::new(left), Box::new(low), Box::new(high), negated));
        }
        if self.eat_keyword("like") {
            let pattern = self.additive()?;
            return Ok(Expr::Like(Box::new(left), Box::new(pattern), negated));
        }
        if negated {
            return Err("Syntax error, expected IN, BETWEEN or LIKE after NOT.".into());
        }
        for op in ["=", "!=", "<>", "<=", ">=", "<", ">"] {
            if self.eat_op(op) {
                let right = self.additive()?;
                let op = if op == "<>" { "!=" } else { op };
                return Ok(Expr::Binary(op, Box::new(left), Box::new(right)));
            }
        }
        Ok(left)
    }

    fn additive(&mut self) -> Result<Expr, String> {
        let mut left = self.multiplicative()?;
        loop {
            let op = if self.eat_op("+") {
                "+"
            } else if self.eat_op("-") {
                "-"
            } else if self.eat_op("||") {
                "||"
            } else {
                return Ok(left);
            };
            left = Expr::Binary(op, Box::new(left), Box::new(self.multiplicative()?));
        }
    }

    fn multiplicative(&mut self) -> Result<Expr, String> {
        let mut left = self.unary()?;
        loop {
            let op = if self.eat_op("*") {
                "*"
            } else if self.eat_op("/") {
                "/"
            } else if self.eat_op("%") {
                "%"
            } else {
                return Ok(left);
            };
            left = Expr::Binary(op, Box::new(left), Box::new(self.unary()?));
        }
    }

    fn unary(&mut self) -> Result<Expr, String> {
        if self.eat_op("-") {
            return Ok(Expr::Unary("-", Box::new(self.unary()?)));
        }
        if self.eat_op("+") {
            return self.unary();
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.primary()?;
        loop {
            if self.eat_op(".") {
                let name = match self.next() {
                    Some(Token::Ident(name)) => name,
                    other => return Err(format!("Syntax error, expected a property name, found {other:?}.")),
                };
                expr = Expr::Member(Box::new(expr), Box::new(Expr::Literal(json!(name))));
            } else if self.eat_op("[") {
                let index = self.expression()?;
                self.expect_op("]")?;
                expr = Expr::Member(Box::new(expr), Box::new(index));
            } else {
                return Ok(expr);
            }
        }
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.next() {
            Some(Token::Num(n)) => Ok(Expr::Literal(number(n))),
            Some(Token::Str(s)) => Ok(Expr::Literal(json!(s))),
            Some(Token::Param(p)) => Ok(Expr::Param(p)),
            Some(Token::Op("(")) => {
                if self.is_keyword("select") {
                    let query = self.query()?;
                    self.expect_op(")")?;
                    return Ok(Expr::Exists(Box::new(query)));
                }
                let inner = self.expression()?;
                self.expect_op(")")?;
                Ok(inner)
            }
            Some(Token::Op("[")) => {
                let mut items = Vec::new();
                if !self.eat_op("]") {
                    loop {
                        items.push(self.expression()?);
                        if !self.eat_op(",") {
                            break;
                        }
                    }
                    self.expect_op("]")?;
                }
                Ok(Expr::Array(items))
            }
            Some(Token::Op("{")) => {
                let mut fields = Vec::new();
                if !self.eat_op("}") {
                    loop {
                        let name = match self.next() {
                            Some(Token::Ident(name)) | Some(Token::Str(name)) => name,
                            other => return Err(format!("Syntax error, expected a property name, found {other:?}.")),
                        };
                        self.expect_op(":")?;
                        fields.push((name, self.expression()?));
                        if !self.eat_op(",") {
                            break;
                        }
                    }
                    self.expect_op("}")?;
                }
                Ok(Expr::Object(fields))
            }
            Some(Token::Ident(word)) => {
                let lower = word.to_ascii_lowercase();
                match lower.as_str() {
                    "true" => return Ok(Expr::Literal(json!(true))),
                    "false" => return Ok(Expr::Literal(json!(false))),
                    "null" => return Ok(Expr::Literal(Value::Null)),
                    "undefined" => return Ok(Expr::Undefined),
                    "exists" => {
                        self.expect_op("(")?;
                        let query = self.query()?;
                        self.expect_op(")")?;
                        return Ok(Expr::Call("exists".into(), vec![Expr::Exists(Box::new(query))]));
                    }
                    // `ARRAY(SELECT ...)`: every value of a subquery, as an array.
                    "array" if self.subquery_follows() => {
                        self.expect_op("(")?;
                        let query = self.query()?;
                        self.expect_op(")")?;
                        return Ok(Expr::Call("array".into(), vec![Expr::Exists(Box::new(query))]));
                    }
                    _ => {}
                }
                if self.eat_op("(") {
                    let mut args = Vec::new();
                    if !self.eat_op(")") {
                        loop {
                            if self.eat_op("*") {
                                args.push(Expr::Literal(json!(1)));
                            } else {
                                args.push(self.expression()?);
                            }
                            if !self.eat_op(",") {
                                break;
                            }
                        }
                        self.expect_op(")")?;
                    }
                    // `udf.name(...)` is not supported; plain names only.
                    return Ok(Expr::Call(lower, args));
                }
                if KEYWORDS.contains(&lower.as_str()) {
                    return Err(format!("Syntax error, incorrect syntax near '{word}'."));
                }
                Ok(Expr::Alias(word))
            }
            other => Err(format!("Syntax error, unexpected {other:?}.")),
        }
    }
}

fn number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9.0e15 {
        json!(n as i64)
    } else {
        Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null)
    }
}

/// Parses `text`, or says why it is not a query.
pub fn parse(text: &str) -> Result<Query, String> {
    let mut parser = Parser { tokens: tokenize(text)?, at: 0 };
    let query = parser.query()?;
    match parser.peek() {
        None => Ok(query),
        Some(token) => Err(format!("Syntax error, unexpected {token:?} after the end of the query.")),
    }
}

/// Bindings in scope while evaluating: the FROM and JOIN aliases.
struct Scope<'a> {
    bindings: Vec<(String, Value)>,
    params: &'a Map<String, Value>,
}

impl Scope<'_> {
    fn lookup(&self, name: &str) -> Val {
        self.bindings.iter().rev().find(|(alias, _)| alias == name).map(|(_, v)| v.clone())
    }
}

fn as_f64(value: &Value) -> Option<f64> {
    value.as_f64()
}

fn type_rank(value: &Val) -> u8 {
    match value {
        None => 0,
        Some(Value::Null) => 1,
        Some(Value::Bool(_)) => 2,
        Some(Value::Number(_)) => 3,
        Some(Value::String(_)) => 4,
        Some(Value::Array(_)) => 5,
        Some(Value::Object(_)) => 6,
    }
}

/// The order ORDER BY sorts in: by type first, then by value.
fn order(a: &Val, b: &Val) -> Ordering {
    match (a, b) {
        (Some(Value::Number(x)), Some(Value::Number(y))) => as_f64(&Value::Number(x.clone()))
            .partial_cmp(&as_f64(&Value::Number(y.clone())))
            .unwrap_or(Ordering::Equal),
        (Some(Value::String(x)), Some(Value::String(y))) => x.cmp(y),
        (Some(Value::Bool(x)), Some(Value::Bool(y))) => x.cmp(y),
        _ => type_rank(a).cmp(&type_rank(b)),
    }
}

/// Comparison as `=`, `<` and friends see it: undefined unless both sides
/// are of the same kind.
fn compare(a: &Val, b: &Val) -> Option<Ordering> {
    match (a.as_ref()?, b.as_ref()?) {
        (Value::Number(x), Value::Number(y)) => x.as_f64()?.partial_cmp(&y.as_f64()?),
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        (Value::Null, Value::Null) => Some(Ordering::Equal),
        (x @ Value::Array(_), y @ Value::Array(_)) | (x @ Value::Object(_), y @ Value::Object(_)) => {
            (x == y).then_some(Ordering::Equal)
        }
        _ => None,
    }
}

fn equal(a: &Val, b: &Val) -> Val {
    match (a, b) {
        (Some(x), Some(y)) => match compare(a, b) {
            Some(ordering) => Some(json!(ordering == Ordering::Equal)),
            // Different kinds are simply unequal, except with undefined.
            None if std::mem::discriminant(x) != std::mem::discriminant(y) => Some(json!(false)),
            None => Some(json!(false)),
        },
        _ => None,
    }
}

fn truthy(value: &Val) -> Option<bool> {
    value.as_ref()?.as_bool()
}

fn like(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    // matched[j]: the pattern so far matches text[..j].
    let mut matched = vec![false; text.len() + 1];
    matched[0] = true;
    let mut i = 0;
    while i < pattern.len() {
        let mut next = vec![false; text.len() + 1];
        match pattern[i] {
            '%' => {
                let mut any = false;
                for j in 0..=text.len() {
                    any |= matched[j];
                    next[j] = any;
                }
            }
            '_' => next[1..].copy_from_slice(&matched[..text.len()]),
            '[' if pattern[i..].contains(&']') => {
                let end = i + pattern[i..].iter().position(|&c| c == ']').unwrap_or(0);
                let class: Vec<char> = pattern[i + 1..end].to_vec();
                let (negate, class) = match class.first() {
                    Some('^') => (true, class[1..].to_vec()),
                    _ => (false, class),
                };
                let member = |c: char| {
                    let mut k = 0;
                    while k < class.len() {
                        if k + 2 < class.len() && class[k + 1] == '-' {
                            if class[k] <= c && c <= class[k + 2] {
                                return true;
                            }
                            k += 3;
                        } else {
                            if class[k] == c {
                                return true;
                            }
                            k += 1;
                        }
                    }
                    false
                };
                for j in 0..text.len() {
                    next[j + 1] = matched[j] && member(text[j]) != negate;
                }
                i = end;
            }
            c => {
                for j in 0..text.len() {
                    next[j + 1] = matched[j] && text[j] == c;
                }
            }
        }
        matched = next;
        i += 1;
    }
    matched[text.len()]
}

fn string_arg(args: &[Val], index: usize) -> Option<&str> {
    args.get(index)?.as_ref()?.as_str()
}

fn bool_arg(args: &[Val], index: usize) -> bool {
    args.get(index).and_then(|v| v.as_ref()).and_then(Value::as_bool).unwrap_or(false)
}

fn num_arg(args: &[Val], index: usize) -> Option<f64> {
    args.get(index)?.as_ref()?.as_f64()
}

fn call(name: &str, args: Vec<Val>) -> Result<Val, String> {
    let lower = |s: &str, ignore: bool| if ignore { s.to_lowercase() } else { s.to_string() };
    Ok(match name {
        "is_defined" => Some(json!(args.first().is_some_and(Option::is_some))),
        "is_null" => Some(json!(matches!(args.first(), Some(Some(Value::Null))))),
        "is_string" => Some(json!(matches!(args.first(), Some(Some(Value::String(_)))))),
        "is_number" => Some(json!(matches!(args.first(), Some(Some(Value::Number(_)))))),
        "is_bool" => Some(json!(matches!(args.first(), Some(Some(Value::Bool(_)))))),
        "is_array" => Some(json!(matches!(args.first(), Some(Some(Value::Array(_)))))),
        "is_object" => Some(json!(matches!(args.first(), Some(Some(Value::Object(_)))))),
        "is_primitive" => Some(json!(matches!(
            args.first(),
            Some(Some(Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null))
        ))),
        "contains" | "startswith" | "endswith" => {
            let (Some(s), Some(t)) = (string_arg(&args, 0), string_arg(&args, 1)) else { return Ok(None) };
            let ignore = bool_arg(&args, 2);
            let (s, t) = (lower(s, ignore), lower(t, ignore));
            Some(json!(match name {
                "contains" => s.contains(&t),
                "startswith" => s.starts_with(&t),
                _ => s.ends_with(&t),
            }))
        }
        "stringequals" => {
            let (Some(s), Some(t)) = (string_arg(&args, 0), string_arg(&args, 1)) else { return Ok(None) };
            let ignore = bool_arg(&args, 2);
            Some(json!(lower(s, ignore) == lower(t, ignore)))
        }
        "lower" => string_arg(&args, 0).map(|s| json!(s.to_lowercase())),
        "upper" => string_arg(&args, 0).map(|s| json!(s.to_uppercase())),
        "trim" => string_arg(&args, 0).map(|s| json!(s.trim())),
        "ltrim" => string_arg(&args, 0).map(|s| json!(s.trim_start())),
        "rtrim" => string_arg(&args, 0).map(|s| json!(s.trim_end())),
        "length" => string_arg(&args, 0).map(|s| json!(s.chars().count())),
        "reverse" => string_arg(&args, 0).map(|s| json!(s.chars().rev().collect::<String>())),
        "concat" => {
            let mut out = String::new();
            for arg in &args {
                out.push_str(arg.as_ref().and_then(Value::as_str).ok_or("CONCAT takes strings.")?);
            }
            Some(json!(out))
        }
        "substring" => {
            let (Some(s), Some(start), Some(len)) = (string_arg(&args, 0), num_arg(&args, 1), num_arg(&args, 2)) else {
                return Ok(None);
            };
            Some(json!(s.chars().skip(start.max(0.0) as usize).take(len.max(0.0) as usize).collect::<String>()))
        }
        "left" | "right" => {
            let (Some(s), Some(n)) = (string_arg(&args, 0), num_arg(&args, 1)) else { return Ok(None) };
            let chars: Vec<char> = s.chars().collect();
            let n = (n.max(0.0) as usize).min(chars.len());
            Some(json!(if name == "left" { chars[..n].iter().collect::<String>() } else { chars[chars.len() - n..].iter().collect() }))
        }
        "index_of" => {
            let (Some(s), Some(t)) = (string_arg(&args, 0), string_arg(&args, 1)) else { return Ok(None) };
            Some(json!(s.find(t).map_or(-1, |byte| s[..byte].chars().count() as i64)))
        }
        "replace" => {
            let (Some(s), Some(from), Some(to)) = (string_arg(&args, 0), string_arg(&args, 1), string_arg(&args, 2)) else {
                return Ok(None);
            };
            Some(json!(s.replace(from, to)))
        }
        "tostring" => args.first().cloned().flatten().map(|v| match v {
            Value::String(s) => json!(s),
            other => json!(other.to_string()),
        }),
        "array_contains" => {
            let (Some(Some(Value::Array(items))), Some(Some(needle))) = (args.first(), args.get(1)) else { return Ok(None) };
            let partial = bool_arg(&args, 2);
            Some(json!(items.iter().any(|item| {
                if partial {
                    match (item, needle) {
                        (Value::Object(i), Value::Object(n)) => n.iter().all(|(k, v)| i.get(k) == Some(v)),
                        _ => item == needle,
                    }
                } else {
                    item == needle
                }
            })))
        }
        "array_length" => match args.first() {
            Some(Some(Value::Array(items))) => Some(json!(items.len())),
            _ => None,
        },
        "array_concat" => {
            let mut out = Vec::new();
            for arg in &args {
                match arg {
                    Some(Value::Array(items)) => out.extend(items.iter().cloned()),
                    _ => return Ok(None),
                }
            }
            Some(json!(out))
        }
        "array_slice" => {
            let (Some(Some(Value::Array(items))), Some(start)) = (args.first(), num_arg(&args, 1)) else { return Ok(None) };
            let len = items.len() as i64;
            let start = start as i64;
            let start = if start < 0 { (len + start).max(0) } else { start.min(len) } as usize;
            let take = num_arg(&args, 2).map_or(items.len(), |n| n.max(0.0) as usize);
            Some(json!(items.iter().skip(start).take(take).cloned().collect::<Vec<_>>()))
        }
        "abs" | "floor" | "ceiling" | "round" | "sqrt" | "trunc" => {
            let Some(n) = num_arg(&args, 0) else { return Ok(None) };
            Some(number(match name {
                "abs" => n.abs(),
                "floor" => n.floor(),
                "ceiling" => n.ceil(),
                "round" => n.round(),
                "sqrt" => n.sqrt(),
                _ => n.trunc(),
            }))
        }
        "power" => {
            let (Some(a), Some(b)) = (num_arg(&args, 0), num_arg(&args, 1)) else { return Ok(None) };
            Some(number(a.powf(b)))
        }
        "getcurrentdatetime" => Some(json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true))),
        "getcurrenttimestamp" => Some(json!(chrono::Utc::now().timestamp_millis())),
        "exists" => args.into_iter().next().flatten(),
        other => return Err(format!("'{other}' is not a recognized built-in function name.")),
    })
}

const AGGREGATES: [&str; 5] = ["count", "sum", "min", "max", "avg"];

fn has_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Call(name, args) => AGGREGATES.contains(&name.as_str()) || args.iter().any(has_aggregate),
        Expr::Member(a, b) | Expr::Binary(_, a, b) | Expr::Like(a, b, _) => has_aggregate(a) || has_aggregate(b),
        Expr::Unary(_, a) => has_aggregate(a),
        Expr::Object(fields) => fields.iter().any(|(_, e)| has_aggregate(e)),
        Expr::Array(items) | Expr::In(_, items, _) => items.iter().any(has_aggregate),
        Expr::Ternary(a, b, c) | Expr::Between(a, b, c, _) => has_aggregate(a) || has_aggregate(b) || has_aggregate(c),
        _ => false,
    }
}

/// Evaluates `expr` for one row; `group` is the rows of the group being
/// aggregated, when there is one.
fn eval(expr: &Expr, scope: &Scope<'_>, group: Option<&[Scope<'_>]>) -> Result<Val, String> {
    Ok(match expr {
        Expr::Literal(value) => Some(value.clone()),
        Expr::Undefined => None,
        Expr::Param(name) => Some(scope.params.get(name).cloned().ok_or_else(|| format!("The parameter {name} is not defined."))?),
        Expr::Alias(name) => scope.lookup(name),
        Expr::Member(target, key) => {
            let target = eval(target, scope, group)?;
            let key = eval(key, scope, group)?;
            match (target, key) {
                (Some(Value::Object(map)), Some(Value::String(k))) => map.get(&k).cloned(),
                (Some(Value::Array(items)), Some(Value::Number(n))) => n.as_u64().and_then(|i| items.get(i as usize).cloned()),
                _ => None,
            }
        }
        Expr::Object(fields) => {
            let mut map = Map::new();
            for (name, value) in fields {
                if let Some(value) = eval(value, scope, group)? {
                    map.insert(name.clone(), value);
                }
            }
            Some(Value::Object(map))
        }
        Expr::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                // Undefined elements are left out of an array literal.
                if let Some(value) = eval(item, scope, group)? {
                    out.push(value);
                }
            }
            Some(Value::Array(out))
        }
        Expr::Unary(op, inner) => {
            let value = eval(inner, scope, group)?;
            match *op {
                "not" => truthy(&value).map(|b| json!(!b)),
                _ => value.as_ref().and_then(Value::as_f64).map(|n| number(-n)),
            }
        }
        Expr::Binary(op, left, right) => {
            let a = eval(left, scope, group)?;
            match *op {
                "and" => {
                    if truthy(&a) == Some(false) {
                        return Ok(Some(json!(false)));
                    }
                    let b = eval(right, scope, group)?;
                    match (truthy(&a), truthy(&b)) {
                        (_, Some(false)) => Some(json!(false)),
                        (Some(true), Some(true)) => Some(json!(true)),
                        _ => None,
                    }
                }
                "or" => {
                    if truthy(&a) == Some(true) {
                        return Ok(Some(json!(true)));
                    }
                    let b = eval(right, scope, group)?;
                    match (truthy(&a), truthy(&b)) {
                        (_, Some(true)) => Some(json!(true)),
                        (Some(false), Some(false)) => Some(json!(false)),
                        _ => None,
                    }
                }
                "??" => match a {
                    Some(value) => Some(value),
                    None => eval(right, scope, group)?,
                },
                _ => {
                    let b = eval(right, scope, group)?;
                    match *op {
                        "=" => equal(&a, &b),
                        "!=" => equal(&a, &b).map(|v| json!(!v.as_bool().unwrap_or(false))),
                        "<" | "<=" | ">" | ">=" => compare(&a, &b).map(|o| {
                            json!(match *op {
                                "<" => o.is_lt(),
                                "<=" => o.is_le(),
                                ">" => o.is_gt(),
                                _ => o.is_ge(),
                            })
                        }),
                        "||" => match (&a, &b) {
                            (Some(Value::String(x)), Some(Value::String(y))) => Some(json!(format!("{x}{y}"))),
                            _ => None,
                        },
                        _ => match (a.as_ref().and_then(Value::as_f64), b.as_ref().and_then(Value::as_f64)) {
                            (Some(x), Some(y)) => match *op {
                                "+" => Some(number(x + y)),
                                "-" => Some(number(x - y)),
                                "*" => Some(number(x * y)),
                                "/" if y != 0.0 => Some(number(x / y)),
                                "%" if y != 0.0 => Some(number(x % y)),
                                _ => None,
                            },
                            _ => None,
                        },
                    }
                }
            }
        }
        Expr::In(target, items, negated) => {
            let value = eval(target, scope, group)?;
            if value.is_none() {
                return Ok(None);
            }
            let mut found = false;
            for item in items {
                if equal(&value, &eval(item, scope, group)?).and_then(|v| v.as_bool()) == Some(true) {
                    found = true;
                    break;
                }
            }
            Some(json!(found != *negated))
        }
        Expr::Between(target, low, high, negated) => {
            let value = eval(target, scope, group)?;
            let (low, high) = (eval(low, scope, group)?, eval(high, scope, group)?);
            match (compare(&value, &low), compare(&value, &high)) {
                (Some(l), Some(h)) => Some(json!((l.is_ge() && h.is_le()) != *negated)),
                _ => None,
            }
        }
        Expr::Like(target, pattern, negated) => match (eval(target, scope, group)?, eval(pattern, scope, group)?) {
            (Some(Value::String(text)), Some(Value::String(pattern))) => Some(json!(like(&text, &pattern) != *negated)),
            _ => None,
        },
        Expr::Ternary(condition, then, otherwise) => {
            if truthy(&eval(condition, scope, group)?) == Some(true) {
                eval(then, scope, group)?
            } else {
                eval(otherwise, scope, group)?
            }
        }
        Expr::Call(name, args) if AGGREGATES.contains(&name.as_str()) => {
            let rows = group.ok_or_else(|| format!("The aggregate {} cannot be used here.", name.to_uppercase()))?;
            let arg = args.first().ok_or("An aggregate takes one argument.")?;
            let mut values = Vec::new();
            for row in rows {
                if let Some(value) = eval(arg, row, None)? {
                    values.push(value);
                }
            }
            aggregate(name, values)
        }
        Expr::Call(name, args) if name == "exists" => {
            let Some(Expr::Exists(query)) = args.first() else { return Ok(None) };
            Some(json!(!subquery(query, scope)?.is_empty()))
        }
        Expr::Call(name, args) if name == "array" && matches!(args.first(), Some(Expr::Exists(_))) => {
            let Some(Expr::Exists(query)) = args.first() else { return Ok(None) };
            Some(Value::Array(subquery(query, scope)?))
        }
        Expr::Exists(query) => {
            // A scalar subquery: its single value.
            subquery(query, scope)?.into_iter().next()
        }
        Expr::Call(name, args) => {
            let mut values = Vec::with_capacity(args.len());
            for arg in args {
                values.push(eval(arg, scope, group)?);
            }
            call(name, values)?
        }
    })
}

fn subquery(query: &Query, scope: &Scope<'_>) -> Result<Vec<Value>, String> {
    // A subquery runs over the outer row's bindings: its FROM is an alias
    // or a path into them.
    let inner = Scope { bindings: scope.bindings.clone(), params: scope.params };
    let source = match &query.from_in {
        Some(path) => eval(path, &inner, None)?,
        None => inner.lookup(&query.alias),
    };
    let rows: Vec<Value> = match source {
        Some(Value::Array(items)) if query.from_in.is_some() => items,
        Some(value) => vec![value],
        None => vec![],
    };
    let mut adjusted = query.clone();
    adjusted.from_in = None;
    run_rows(&adjusted, rows, scope.params, &scope.bindings)
}

fn aggregate(name: &str, values: Vec<Value>) -> Val {
    match name {
        "count" => Some(json!(values.len())),
        "sum" => {
            let numbers: Option<Vec<f64>> = values.iter().map(Value::as_f64).collect();
            numbers.map(|n| number(n.iter().sum()))
        }
        "avg" => {
            let numbers: Option<Vec<f64>> = values.iter().map(Value::as_f64).collect();
            numbers.filter(|n| !n.is_empty()).map(|n| number(n.iter().sum::<f64>() / n.len() as f64))
        }
        _ => {
            let mut best: Val = None;
            for value in values {
                let candidate = Some(value);
                let better = match &best {
                    None => true,
                    Some(_) if name == "min" => order(&candidate, &best).is_lt(),
                    Some(_) => order(&candidate, &best).is_gt(),
                };
                if better {
                    best = candidate;
                }
            }
            best
        }
    }
}

fn integer(expr: &Option<Expr>, params: &Map<String, Value>) -> Result<Option<usize>, String> {
    let Some(expr) = expr else { return Ok(None) };
    let scope = Scope { bindings: vec![], params };
    match eval(expr, &scope, None)? {
        Some(Value::Number(n)) if n.as_u64().is_some() => Ok(n.as_u64().map(|n| n as usize)),
        _ => Err("TOP, OFFSET and LIMIT take a non-negative integer.".into()),
    }
}

/// The name a projected expression is given when the query names none.
fn implicit_name(expr: &Expr, index: usize) -> String {
    match expr {
        Expr::Member(_, key) => match key.as_ref() {
            Expr::Literal(Value::String(name)) => name.clone(),
            _ => format!("${}", index + 1),
        },
        Expr::Alias(name) => name.clone(),
        _ => format!("${}", index + 1),
    }
}

fn project(query: &Query, scope: &Scope<'_>, group: Option<&[Scope<'_>]>) -> Result<Val, String> {
    Ok(match &query.projection {
        Projection::Star => {
            if query.joins.is_empty() {
                scope.lookup(&query.alias)
            } else {
                let mut map = Map::new();
                for (alias, value) in &scope.bindings {
                    map.insert(alias.clone(), value.clone());
                }
                Some(Value::Object(map))
            }
        }
        Projection::Value(expr) => eval(expr, scope, group)?,
        Projection::Fields(fields) => {
            let mut map = Map::new();
            for (index, (alias, expr)) in fields.iter().enumerate() {
                if let Some(value) = eval(expr, scope, group)? {
                    map.insert(alias.clone().unwrap_or_else(|| implicit_name(expr, index)), value);
                }
            }
            Some(Value::Object(map))
        }
    })
}

/// Runs `query` over `documents`, returning its results in order.
pub fn run(query: &Query, documents: Vec<Value>, params: &Map<String, Value>) -> Result<Vec<Value>, String> {
    run_rows(query, documents, params, &[])
}

fn run_rows(query: &Query, documents: Vec<Value>, params: &Map<String, Value>, outer: &[(String, Value)]) -> Result<Vec<Value>, String> {
    // Every row is the set of bindings: the FROM alias, then each JOIN's.
    let mut rows: Vec<Scope<'_>> = Vec::new();
    for document in documents {
        let base = outer.to_vec();
        let starting: Vec<Vec<(String, Value)>> = match &query.from_in {
            Some(path) => {
                let scope = Scope { bindings: vec![(query.alias.clone(), document.clone())], params };
                // `FROM x IN c.items`: the path is relative to the document,
                // whatever its root alias is called.
                let path = rebase(path, &query.alias);
                match eval(&path, &scope, None)? {
                    Some(Value::Array(items)) => items.into_iter().map(|item| vec![(query.alias.clone(), item)]).collect(),
                    _ => vec![],
                }
            }
            None => vec![vec![(query.alias.clone(), document)]],
        };
        let mut expanded = starting;
        for join in &query.joins {
            let mut next = Vec::new();
            for bindings in expanded {
                let scope = Scope { bindings: [base.clone(), bindings.clone()].concat(), params };
                if let Some(Value::Array(items)) = eval(&join.source, &scope, None)? {
                    for item in items {
                        let mut more = bindings.clone();
                        more.push((join.alias.clone(), item));
                        next.push(more);
                    }
                }
            }
            expanded = next;
        }
        for bindings in expanded {
            rows.push(Scope { bindings: [base.clone(), bindings].concat(), params });
        }
    }
    if let Some(filter) = &query.filter {
        let mut kept = Vec::new();
        for row in rows {
            if truthy(&eval(filter, &row, None)?) == Some(true) {
                kept.push(row);
            }
        }
        rows = kept;
    }

    let aggregated = match &query.projection {
        Projection::Value(expr) => has_aggregate(expr),
        Projection::Fields(fields) => fields.iter().any(|(_, e)| has_aggregate(e)),
        Projection::Star => false,
    };
    let mut results: Vec<(Vec<Val>, Value)> = Vec::new();
    if !query.group_by.is_empty() {
        let mut groups: Vec<(Vec<Val>, Vec<Scope<'_>>)> = Vec::new();
        for row in rows {
            let mut key = Vec::new();
            for expr in &query.group_by {
                key.push(eval(expr, &row, None)?);
            }
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, members)) => members.push(row),
                None => groups.push((key, vec![row])),
            }
        }
        for (_, members) in &groups {
            if let Some(value) = project(query, &members[0], Some(members))? {
                results.push((vec![], value));
            }
        }
    } else if aggregated {
        let representative = rows.first().map(|r| Scope { bindings: r.bindings.clone(), params }).unwrap_or(Scope { bindings: vec![], params });
        if let Some(value) = project(query, &representative, Some(&rows))? {
            results.push((vec![], value));
        }
    } else {
        for row in &rows {
            let mut keys = Vec::new();
            for (expr, _) in &query.order_by {
                keys.push(eval(expr, row, None)?);
            }
            if let Some(value) = project(query, row, None)? {
                results.push((keys, value));
            }
        }
        if !query.order_by.is_empty() {
            results.sort_by(|(a, _), (b, _)| {
                for (index, (_, descending)) in query.order_by.iter().enumerate() {
                    let ordering = order(&a[index], &b[index]);
                    let ordering = if *descending { ordering.reverse() } else { ordering };
                    if ordering != Ordering::Equal {
                        return ordering;
                    }
                }
                Ordering::Equal
            });
        }
    }
    let mut values: Vec<Value> = results.into_iter().map(|(_, v)| v).collect();
    if query.distinct {
        let mut unique: Vec<Value> = Vec::new();
        for value in values {
            if !unique.contains(&value) {
                unique.push(value);
            }
        }
        values = unique;
    }
    if let Some(offset) = integer(&query.offset, params)? {
        values = values.into_iter().skip(offset).collect();
    }
    if let Some(limit) = integer(&query.limit, params)? {
        values.truncate(limit);
    }
    if let Some(top) = integer(&query.top, params)? {
        values.truncate(top);
    }
    Ok(values)
}

/// `c.items` in `FROM x IN c.items` names the document as `c`; it is
/// evaluated with the document bound to the FROM alias.
fn rebase(path: &Expr, alias: &str) -> Expr {
    match path {
        Expr::Alias(_) => Expr::Alias(alias.to_string()),
        Expr::Member(target, key) => Expr::Member(Box::new(rebase(target, alias)), key.clone()),
        other => other.clone(),
    }
}

/// What the SDKs' query plan needs to know about a query.
pub struct Plan {
    pub has_select_value: bool,
}

pub fn plan(query: &Query) -> Plan {
    Plan { has_select_value: matches!(query.projection, Projection::Value(_)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docs() -> Vec<Value> {
        vec![
            json!({"id": "1", "name": "Ann", "age": 31, "city": "Oslo", "tags": ["a", "b"], "items": [{"sku": "x", "qty": 2}]}),
            json!({"id": "2", "name": "Bob", "age": 25, "city": "Rome", "tags": ["b"], "items": [{"sku": "y", "qty": 1}, {"sku": "x", "qty": 5}]}),
            json!({"id": "3", "name": "Cid", "age": 40, "city": "Oslo", "tags": [], "nick": null}),
        ]
    }

    fn q(text: &str) -> Vec<Value> {
        q_with(text, Map::new())
    }

    fn q_with(text: &str, params: Map<String, Value>) -> Vec<Value> {
        let query = parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        run(&query, docs(), &params).unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    fn ids(values: Vec<Value>) -> Vec<String> {
        values.iter().map(|v| v["id"].as_str().unwrap_or_default().to_string()).collect()
    }

    #[test]
    fn filters_and_projections() {
        assert_eq!(ids(q("SELECT * FROM c")), ["1", "2", "3"]);
        assert_eq!(ids(q("SELECT * FROM c WHERE c.age > 30")), ["1", "3"]);
        assert_eq!(ids(q("SELECT * FROM c WHERE c.city = 'Oslo' AND c.age < 35")), ["1"]);
        assert_eq!(ids(q("select * from root r where r.name in ('Bob', 'Cid')")), ["2", "3"]);
        assert_eq!(ids(q("SELECT * FROM c WHERE c.age BETWEEN 25 AND 31")), ["1", "2"]);
        assert_eq!(ids(q("SELECT * FROM c WHERE STARTSWITH(c.name, 'a', true)")), ["1"]);
        assert_eq!(ids(q("SELECT * FROM c WHERE ARRAY_CONTAINS(c.tags, 'b')")), ["1", "2"]);
        assert_eq!(ids(q("SELECT * FROM c WHERE IS_DEFINED(c.nick)")), ["3"]);
        assert_eq!(ids(q("SELECT * FROM c WHERE c.nick = null")), ["3"]);
        assert_eq!(ids(q("SELECT * FROM c WHERE c.missing = 1 OR c.id = '2'")), ["2"]);
        assert_eq!(ids(q("SELECT * FROM c WHERE NOT (c.missing = 1)")), Vec::<String>::new());
        assert_eq!(ids(q("SELECT * FROM c WHERE c.name LIKE '%o%'")), ["2"]);
        assert_eq!(ids(q("SELECT * FROM c WHERE c['city'] != 'Oslo'")), ["2"]);
        assert_eq!(q("SELECT c.name, c.age FROM c WHERE c.id = '1'"), [json!({"name": "Ann", "age": 31})]);
        assert_eq!(q("SELECT c.name AS n, c.missing FROM c WHERE c.id = '1'"), [json!({"n": "Ann"})]);
        assert_eq!(q("SELECT VALUE c.name FROM c"), [json!("Ann"), json!("Bob"), json!("Cid")]);
        assert_eq!(q("SELECT VALUE {n: c.name, old: c.age > 30} FROM c WHERE c.id = '2'"), [json!({"n": "Bob", "old": false})]);
        assert_eq!(q("SELECT VALUE c.tags[0] FROM c"), [json!("a"), json!("b")]);
        assert_eq!(q("SELECT VALUE UPPER(c.name) || '!' FROM c WHERE c.id = '3'"), [json!("CID!")]);
        assert_eq!(q("SELECT VALUE c.age * 2 + 1 FROM c WHERE c.id = '2'"), [json!(51)]);
        assert_eq!(q("SELECT VALUE c.nick ?? 'none' FROM c WHERE c.id = '1'"), [json!("none")]);
        assert_eq!(q("SELECT VALUE c.age > 30 ? 'old' : 'young' FROM c WHERE c.id = '2'"), [json!("young")]);
    }

    #[test]
    fn ordering_paging_and_distinct() {
        assert_eq!(ids(q("SELECT * FROM c ORDER BY c.age DESC")), ["3", "1", "2"]);
        assert_eq!(ids(q("SELECT * FROM c ORDER BY c.city, c.age DESC")), ["3", "1", "2"]);
        assert_eq!(ids(q("SELECT TOP 2 * FROM c ORDER BY c.name")), ["1", "2"]);
        assert_eq!(ids(q("SELECT * FROM c ORDER BY c.age OFFSET 1 LIMIT 1")), ["1"]);
        assert_eq!(q("SELECT DISTINCT VALUE c.city FROM c"), [json!("Oslo"), json!("Rome")]);
    }

    #[test]
    fn aggregates_and_grouping() {
        assert_eq!(q("SELECT VALUE COUNT(1) FROM c"), [json!(3)]);
        assert_eq!(q("SELECT VALUE COUNT(1) FROM c WHERE c.age > 100"), [json!(0)]);
        assert_eq!(q("SELECT COUNT(1) AS n, MAX(c.age) AS oldest, MIN(c.name) AS first FROM c"), [json!({"n": 3, "oldest": 40, "first": "Ann"})]);
        assert_eq!(q("SELECT VALUE SUM(c.age) FROM c"), [json!(96)]);
        assert_eq!(q("SELECT VALUE AVG(c.age) FROM c WHERE c.city = 'Oslo'"), [json!(35.5)]);
        assert_eq!(
            q("SELECT c.city, COUNT(1) AS n FROM c GROUP BY c.city"),
            [json!({"city": "Oslo", "n": 2}), json!({"city": "Rome", "n": 1})]
        );
    }

    #[test]
    fn joins_and_parameters() {
        assert_eq!(
            q("SELECT c.id, i.sku FROM c JOIN i IN c.items WHERE i.qty > 1"),
            [json!({"id": "1", "sku": "x"}), json!({"id": "2", "sku": "x"})]
        );
        assert_eq!(q("SELECT VALUE t FROM c JOIN t IN c.tags"), [json!("a"), json!("b"), json!("b")]);
        assert_eq!(q("SELECT VALUE i.sku FROM i IN c.items"), [json!("x"), json!("y"), json!("x")]);
        let mut params = Map::new();
        params.insert("@city".into(), json!("Rome"));
        params.insert("@min".into(), json!(20));
        assert_eq!(ids(q_with("SELECT * FROM c WHERE c.city = @city AND c.age >= @min", params)), ["2"]);
        assert_eq!(ids(q("SELECT * FROM c WHERE EXISTS(SELECT VALUE i FROM i IN c.items WHERE i.qty = 5)")), ["2"]);
    }

    #[test]
    fn errors() {
        for bad in ["SELECT", "SELECT * FROM", "SELECT * FROM c WHERE", "SELECT * FROM c WHERE c.a = 'x", "SELECT * FROM c ORDER c.a", "FROM c"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        let query = parse("SELECT * FROM c WHERE NOPE(c.a)").unwrap();
        assert!(run(&query, docs(), &Map::new()).is_err());
        let query = parse("SELECT * FROM c WHERE c.a = @missing").unwrap();
        assert!(run(&query, docs(), &Map::new()).is_err());
    }
}
