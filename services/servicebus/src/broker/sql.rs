//! The SQL dialect of subscription rules: filters (`color = 'red' AND
//! quantity > 10`) and actions (`SET priority = 'high'; REMOVE draft`).
//!
//! Supported, as Service Bus documents it: comparison (`=`, `<>`, `!=`, `<`,
//! `<=`, `>`, `>=`), `AND`, `OR`, `NOT`, parentheses, arithmetic (`+`, `-`,
//! `*`, `/`, `%`), `IS [NOT] NULL`, `[NOT] IN (...)`, `[NOT] LIKE` with `%`, `_`
//! and `ESCAPE`, `EXISTS(property)`, string, integer, decimal and boolean
//! literals, `NULL`, and property references: bare or `user.` names are
//! application properties, `sys.` names are system properties, and `[...]`
//! quotes a name. Keywords are case-insensitive. A comparison with a missing
//! property is unknown, and unknown never matches.

use fe2o3_amqp_types::messaging::MessageId;
use fe2o3_amqp_types::primitives::SimpleValue;

use super::store::AmqpMessage;

/// A value an expression evaluates to.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Val {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Ident(String),
    Str(String),
    Int(i64),
    Float(f64),
    Op(&'static str),
    LParen,
    RParen,
    Comma,
    Semicolon,
}

fn tokenize(text: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '(' => {
                tokens.push(Token::LParen);
                i += 1
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1
            }
            ',' => {
                tokens.push(Token::Comma);
                i += 1
            }
            ';' => {
                tokens.push(Token::Semicolon);
                i += 1
            }
            '\'' => {
                let mut value = String::new();
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return Err("A string literal is not closed.".into()),
                        Some('\'') if chars.get(i + 1) == Some(&'\'') => {
                            value.push('\'');
                            i += 2;
                        }
                        Some('\'') => {
                            i += 1;
                            break;
                        }
                        Some(&c) => {
                            value.push(c);
                            i += 1;
                        }
                    }
                }
                tokens.push(Token::Str(value));
            }
            '[' => {
                let end = chars[i..].iter().position(|&c| c == ']').ok_or("A [name] is not closed.")?;
                tokens.push(Token::Ident(chars[i + 1..i + end].iter().collect()));
                i += end + 1;
            }
            c if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit())) => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                if text.contains('.') {
                    tokens.push(Token::Float(text.parse().map_err(|_| format!("'{text}' is not a number."))?));
                } else {
                    tokens.push(Token::Int(text.parse().map_err(|_| format!("'{text}' is not a number."))?));
                }
            }
            c if c.is_alphabetic() || c == '_' || c == '$' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '$' | '.' | '-')) {
                    i += 1;
                }
                // A trailing '-' belongs to arithmetic, not the name.
                while i > start + 1 && chars[i - 1] == '-' {
                    i -= 1;
                }
                tokens.push(Token::Ident(chars[start..i].iter().collect()));
            }
            _ => {
                let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
                let op = ["<>", "!=", "<=", ">="].into_iter().find(|op| *op == two);
                if let Some(op) = op {
                    tokens.push(Token::Op(op));
                    i += 2;
                } else {
                    let op = ["=", "<", ">", "+", "-", "*", "/", "%"]
                        .into_iter()
                        .find(|op| op.starts_with(c))
                        .ok_or_else(|| format!("Unexpected character '{c}'."))?;
                    tokens.push(Token::Op(op));
                    i += 1;
                }
            }
        }
    }
    Ok(tokens)
}

/// A property a rule reads or writes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Property {
    User(String),
    System(String),
}

#[derive(Clone, Debug, PartialEq)]
enum Expr {
    Literal(Val),
    Property(Property),
    Not(Box<Expr>),
    Neg(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Compare(&'static str, Box<Expr>, Box<Expr>),
    Arith(&'static str, Box<Expr>, Box<Expr>),
    IsNull(Box<Expr>, bool),
    In(Box<Expr>, Vec<Expr>, bool),
    Like(Box<Expr>, String, Option<char>, bool),
    Exists(Property),
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

fn keyword(token: Option<&Token>, word: &str) -> bool {
    matches!(token, Some(Token::Ident(name)) if name.eq_ignore_ascii_case(word))
}

const RESERVED: &[&str] = &["and", "or", "not", "is", "null", "in", "like", "escape", "exists", "true", "false", "set", "remove"];

fn property(name: &str) -> Property {
    let lower = name.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("sys.") {
        Property::System(rest.to_string())
    } else if lower.starts_with("user.") {
        Property::User(name[5..].to_string())
    } else {
        Property::User(name.to_string())
    }
}

impl Parser {
    fn new(text: &str) -> Result<Self, String> {
        Ok(Parser { tokens: tokenize(text)?, at: 0 })
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at).cloned();
        self.at += 1;
        token
    }

    fn eat_keyword(&mut self, word: &str) -> bool {
        if keyword(self.peek(), word) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: Token) -> Result<(), String> {
        match self.next() {
            Some(t) if t == token => Ok(()),
            other => Err(format!("Expected {token:?}, found {other:?}.")),
        }
    }

    fn or(&mut self) -> Result<Expr, String> {
        let mut left = self.and()?;
        while self.eat_keyword("or") {
            left = Expr::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, String> {
        let mut left = self.not()?;
        while self.eat_keyword("and") {
            left = Expr::And(Box::new(left), Box::new(self.not()?));
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Expr, String> {
        if self.eat_keyword("not") {
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.predicate()
    }

    fn predicate(&mut self) -> Result<Expr, String> {
        let left = self.additive()?;
        if self.eat_keyword("is") {
            let negated = self.eat_keyword("not");
            if !self.eat_keyword("null") {
                return Err("Expected NULL after IS.".into());
            }
            return Ok(Expr::IsNull(Box::new(left), negated));
        }
        let negated = self.eat_keyword("not");
        if self.eat_keyword("in") {
            self.expect(Token::LParen)?;
            let mut items = vec![self.additive()?];
            while self.peek() == Some(&Token::Comma) {
                self.at += 1;
                items.push(self.additive()?);
            }
            self.expect(Token::RParen)?;
            return Ok(Expr::In(Box::new(left), items, negated));
        }
        if self.eat_keyword("like") {
            let Some(Token::Str(pattern)) = self.next() else {
                return Err("LIKE needs a string pattern.".into());
            };
            let escape = if self.eat_keyword("escape") {
                match self.next() {
                    Some(Token::Str(e)) if e.chars().count() == 1 => e.chars().next(),
                    _ => return Err("ESCAPE needs a one-character string.".into()),
                }
            } else {
                None
            };
            return Ok(Expr::Like(Box::new(left), pattern, escape, negated));
        }
        if negated {
            return Err("Expected IN or LIKE after NOT.".into());
        }
        if let Some(Token::Op(op @ ("=" | "<>" | "!=" | "<" | "<=" | ">" | ">="))) = self.peek().cloned() {
            self.at += 1;
            let right = self.additive()?;
            return Ok(Expr::Compare(op, Box::new(left), Box::new(right)));
        }
        Ok(left)
    }

    fn additive(&mut self) -> Result<Expr, String> {
        let mut left = self.multiplicative()?;
        while let Some(Token::Op(op @ ("+" | "-"))) = self.peek().cloned() {
            self.at += 1;
            left = Expr::Arith(op, Box::new(left), Box::new(self.multiplicative()?));
        }
        Ok(left)
    }

    fn multiplicative(&mut self) -> Result<Expr, String> {
        let mut left = self.unary()?;
        while let Some(Token::Op(op @ ("*" | "/" | "%"))) = self.peek().cloned() {
            self.at += 1;
            left = Expr::Arith(op, Box::new(left), Box::new(self.unary()?));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Some(Token::Op("-")) => {
                self.at += 1;
                Ok(Expr::Neg(Box::new(self.unary()?)))
            }
            Some(Token::Op("+")) => {
                self.at += 1;
                self.unary()
            }
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.next() {
            Some(Token::LParen) => {
                let inner = self.or()?;
                self.expect(Token::RParen)?;
                Ok(inner)
            }
            Some(Token::Str(s)) => Ok(Expr::Literal(Val::Str(s))),
            Some(Token::Int(n)) => Ok(Expr::Literal(Val::Int(n))),
            Some(Token::Float(n)) => Ok(Expr::Literal(Val::Float(n))),
            Some(Token::Ident(name)) => {
                let lower = name.to_ascii_lowercase();
                match lower.as_str() {
                    "true" => Ok(Expr::Literal(Val::Bool(true))),
                    "false" => Ok(Expr::Literal(Val::Bool(false))),
                    "null" => Ok(Expr::Literal(Val::Null)),
                    "exists" => {
                        self.expect(Token::LParen)?;
                        let Some(Token::Ident(name)) = self.next() else {
                            return Err("EXISTS needs a property name.".into());
                        };
                        self.expect(Token::RParen)?;
                        Ok(Expr::Exists(property(&name)))
                    }
                    _ if RESERVED.contains(&lower.as_str()) => Err(format!("Unexpected keyword '{name}'.")),
                    _ => Ok(Expr::Property(property(&name))),
                }
            }
            other => Err(format!("Unexpected {other:?}.")),
        }
    }

    fn done(&self) -> Result<(), String> {
        match self.peek() {
            None => Ok(()),
            Some(token) => Err(format!("Unexpected {token:?} after the end of the expression.")),
        }
    }
}

/// A parsed filter expression.
pub(crate) struct Filter(Expr);

impl Filter {
    pub fn parse(text: &str) -> Result<Filter, String> {
        let mut parser = Parser::new(text)?;
        let expr = parser.or()?;
        parser.done()?;
        Ok(Filter(expr))
    }

    /// Whether the message matches: unknown (a missing property) does not.
    pub fn matches(&self, message: &AmqpMessage) -> bool {
        truth(&eval(&self.0, message)) == Some(true)
    }
}

/// One statement of a rule action.
enum Statement {
    Set(Property, Expr),
    Remove(Property),
}

/// A parsed rule action.
pub(crate) struct Action(Vec<Statement>);

impl Action {
    pub fn parse(text: &str) -> Result<Action, String> {
        let mut parser = Parser::new(text)?;
        let mut statements = Vec::new();
        loop {
            while parser.peek() == Some(&Token::Semicolon) {
                parser.at += 1;
            }
            if parser.peek().is_none() {
                break;
            }
            if parser.eat_keyword("set") {
                let Some(Token::Ident(name)) = parser.next() else {
                    return Err("SET needs a property name.".into());
                };
                parser.expect(Token::Op("="))?;
                statements.push(Statement::Set(property(&name), parser.or()?));
            } else if parser.eat_keyword("remove") {
                let Some(Token::Ident(name)) = parser.next() else {
                    return Err("REMOVE needs a property name.".into());
                };
                statements.push(Statement::Remove(property(&name)));
            } else {
                return Err(format!("Expected SET or REMOVE, found {:?}.", parser.peek()));
            }
            match parser.peek() {
                None | Some(Token::Semicolon) => {}
                Some(token) => return Err(format!("Unexpected {token:?}; separate statements with ';'.")),
            }
        }
        if statements.is_empty() {
            return Err("The action has no statements.".into());
        }
        for statement in &statements {
            let (Statement::Set(Property::System(name), _) | Statement::Remove(Property::System(name))) = statement else {
                continue;
            };
            if system_setter(name).is_none() {
                return Err(format!("The system property 'sys.{name}' cannot be changed by an action."));
            }
        }
        Ok(Action(statements))
    }

    /// Applies the action to `message`. Each `SET` sees the message as the
    /// statements before it left it.
    pub fn apply(&self, message: &mut AmqpMessage) {
        for statement in &self.0 {
            match statement {
                Statement::Set(target, expr) => {
                    let value = eval(expr, message);
                    write(message, target, Some(value));
                }
                Statement::Remove(target) => write(message, target, None),
            }
        }
    }
}

fn truth(value: &Val) -> Option<bool> {
    match value {
        Val::Bool(b) => Some(*b),
        _ => None,
    }
}

fn id_text(id: &MessageId) -> String {
    match id {
        MessageId::String(text) => text.clone(),
        MessageId::Ulong(number) => number.to_string(),
        MessageId::Uuid(uuid) => uuid::Uuid::from_bytes(*uuid.as_inner()).to_string(),
        MessageId::Binary(bytes) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn from_simple(value: &SimpleValue) -> Val {
    match value {
        SimpleValue::Null => Val::Null,
        SimpleValue::Bool(b) => Val::Bool(*b),
        SimpleValue::Ubyte(n) => Val::Int(*n as i64),
        SimpleValue::Ushort(n) => Val::Int(*n as i64),
        SimpleValue::Uint(n) => Val::Int(*n as i64),
        SimpleValue::Ulong(n) => i64::try_from(*n).map(Val::Int).unwrap_or(Val::Float(*n as f64)),
        SimpleValue::Byte(n) => Val::Int(*n as i64),
        SimpleValue::Short(n) => Val::Int(*n as i64),
        SimpleValue::Int(n) => Val::Int(*n as i64),
        SimpleValue::Long(n) => Val::Int(*n),
        SimpleValue::Float(n) => Val::Float(n.into_inner() as f64),
        SimpleValue::Double(n) => Val::Float(n.into_inner()),
        SimpleValue::String(s) => Val::Str(s.clone()),
        SimpleValue::Symbol(s) => Val::Str(s.0.clone()),
        SimpleValue::Char(c) => Val::Str(c.to_string()),
        SimpleValue::Uuid(u) => Val::Str(uuid::Uuid::from_bytes(*u.as_inner()).to_string()),
        _ => Val::Null,
    }
}

fn to_simple(value: Val) -> SimpleValue {
    match value {
        Val::Null => SimpleValue::Null,
        Val::Bool(b) => SimpleValue::Bool(b),
        Val::Int(n) => SimpleValue::Long(n),
        Val::Float(n) => SimpleValue::Double(n.into()),
        Val::Str(s) => SimpleValue::String(s),
    }
}

fn text(value: Option<String>) -> Val {
    value.map(Val::Str).unwrap_or(Val::Null)
}

/// Reads a property; `Null` when the message does not have it.
fn read(message: &AmqpMessage, target: &Property) -> Val {
    match target {
        Property::User(name) => message
            .application_properties
            .as_ref()
            .and_then(|a| a.0.get(name))
            .map(from_simple)
            .unwrap_or(Val::Null),
        Property::System(name) => {
            let properties = message.properties.as_ref();
            match name.as_str() {
                "messageid" => text(properties.and_then(|p| p.message_id.as_ref()).map(id_text)),
                "correlationid" => text(properties.and_then(|p| p.correlation_id.as_ref()).map(id_text)),
                "to" => text(properties.and_then(|p| p.to.clone())),
                "replyto" => text(properties.and_then(|p| p.reply_to.clone())),
                "label" | "subject" => text(properties.and_then(|p| p.subject.clone())),
                "sessionid" => text(properties.and_then(|p| p.group_id.clone())),
                "replytosessionid" => text(properties.and_then(|p| p.reply_to_group_id.clone())),
                "contenttype" => text(properties.and_then(|p| p.content_type.as_ref()).map(|s| s.0.clone())),
                "deliverycount" => Val::Int(message.header.as_ref().map_or(0, |h| h.delivery_count) as i64),
                _ => Val::Null,
            }
        }
    }
}

type SystemSetter = fn(&mut fe2o3_amqp_types::messaging::Properties, Option<String>);

/// The system properties an action may set or remove.
fn system_setter(name: &str) -> Option<SystemSetter> {
    Some(match name {
        "messageid" => |p, v| p.message_id = v.map(MessageId::String),
        "correlationid" => |p, v| p.correlation_id = v.map(MessageId::String),
        "to" => |p, v| p.to = v,
        "replyto" => |p, v| p.reply_to = v,
        "label" | "subject" => |p, v| p.subject = v,
        "sessionid" => |p, v| p.group_id = v,
        "replytosessionid" => |p, v| p.reply_to_group_id = v,
        "contenttype" => |p, v| p.content_type = v.map(Into::into),
        _ => return None,
    })
}

fn write(message: &mut AmqpMessage, target: &Property, value: Option<Val>) {
    match target {
        Property::User(name) => {
            let properties = message
                .application_properties
                .get_or_insert_with(|| fe2o3_amqp_types::messaging::ApplicationProperties(Default::default()));
            match value {
                Some(value) => {
                    properties.0.insert(name.clone(), to_simple(value));
                }
                None => {
                    properties.0.swap_remove(name);
                }
            }
        }
        Property::System(name) => {
            let Some(set) = system_setter(name) else { return };
            let value = value.and_then(|v| match v {
                Val::Null => None,
                Val::Str(s) => Some(s),
                Val::Int(n) => Some(n.to_string()),
                Val::Float(n) => Some(n.to_string()),
                Val::Bool(b) => Some(b.to_string()),
            });
            set(message.properties.get_or_insert_with(Default::default), value);
        }
    }
}

fn number(value: &Val) -> Option<f64> {
    match value {
        Val::Int(n) => Some(*n as f64),
        Val::Float(n) => Some(*n),
        _ => None,
    }
}

/// Compares two values of the same kind; `None` when they cannot be compared.
fn compare(left: &Val, right: &Val) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (Val::Int(a), Val::Int(b)) => Some(a.cmp(b)),
        (Val::Str(a), Val::Str(b)) => Some(a.cmp(b)),
        (Val::Bool(a), Val::Bool(b)) => Some(a.cmp(b)),
        _ => number(left)?.partial_cmp(&number(right)?),
    }
}

fn like(text: &str, pattern: &str, escape: Option<char>) -> bool {
    // Pattern elements: a literal character, any one (_) or any run (%).
    enum Part {
        Char(char),
        One,
        Many,
    }
    let mut parts = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if Some(c) == escape {
            if let Some(next) = chars.next() {
                parts.push(Part::Char(next));
            }
        } else if c == '%' {
            parts.push(Part::Many);
        } else if c == '_' {
            parts.push(Part::One);
        } else {
            parts.push(Part::Char(c));
        }
    }
    let text: Vec<char> = text.chars().collect();
    // matched[j]: the pattern so far matches text[..j].
    let mut matched = vec![false; text.len() + 1];
    matched[0] = true;
    for part in &parts {
        let mut next = vec![false; text.len() + 1];
        match part {
            Part::Many => {
                let mut any = false;
                for j in 0..=text.len() {
                    any |= matched[j];
                    next[j] = any;
                }
            }
            Part::One => next[1..].copy_from_slice(&matched[..text.len()]),
            Part::Char(c) => {
                for j in 0..text.len() {
                    next[j + 1] = matched[j] && text[j] == *c;
                }
            }
        }
        matched = next;
    }
    matched[text.len()]
}

fn eval(expr: &Expr, message: &AmqpMessage) -> Val {
    match expr {
        Expr::Literal(value) => value.clone(),
        Expr::Property(target) => read(message, target),
        Expr::Exists(target) => Val::Bool(read(message, target) != Val::Null),
        Expr::Not(inner) => match truth(&eval(inner, message)) {
            Some(b) => Val::Bool(!b),
            None => Val::Null,
        },
        Expr::Neg(inner) => match eval(inner, message) {
            Val::Int(n) => Val::Int(-n),
            Val::Float(n) => Val::Float(-n),
            _ => Val::Null,
        },
        Expr::And(left, right) => match (truth(&eval(left, message)), truth(&eval(right, message))) {
            (Some(false), _) | (_, Some(false)) => Val::Bool(false),
            (Some(true), Some(true)) => Val::Bool(true),
            _ => Val::Null,
        },
        Expr::Or(left, right) => match (truth(&eval(left, message)), truth(&eval(right, message))) {
            (Some(true), _) | (_, Some(true)) => Val::Bool(true),
            (Some(false), Some(false)) => Val::Bool(false),
            _ => Val::Null,
        },
        Expr::Compare(op, left, right) => {
            let (left, right) = (eval(left, message), eval(right, message));
            if left == Val::Null || right == Val::Null {
                return Val::Null;
            }
            let ordering = compare(&left, &right);
            if matches!(*op, "=" | "<>" | "!=") {
                let equal = ordering == Some(std::cmp::Ordering::Equal);
                return Val::Bool(if *op == "=" { equal } else { !equal });
            }
            match ordering {
                None => Val::Null,
                Some(ordering) => Val::Bool(match *op {
                    "<" => ordering.is_lt(),
                    "<=" => ordering.is_le(),
                    ">" => ordering.is_gt(),
                    _ => ordering.is_ge(),
                }),
            }
        }
        Expr::Arith(op, left, right) => {
            let (left, right) = (eval(left, message), eval(right, message));
            match (&left, &right, *op) {
                (Val::Str(a), Val::Str(b), "+") => Val::Str(format!("{a}{b}")),
                (Val::Int(a), Val::Int(b), _) => match *op {
                    "+" => a.checked_add(*b).map_or(Val::Null, Val::Int),
                    "-" => a.checked_sub(*b).map_or(Val::Null, Val::Int),
                    "*" => a.checked_mul(*b).map_or(Val::Null, Val::Int),
                    "/" => a.checked_div(*b).map_or(Val::Null, Val::Int),
                    _ => a.checked_rem(*b).map_or(Val::Null, Val::Int),
                },
                _ => match (number(&left), number(&right)) {
                    (Some(a), Some(b)) => Val::Float(match *op {
                        "+" => a + b,
                        "-" => a - b,
                        "*" => a * b,
                        "/" => a / b,
                        _ => a % b,
                    }),
                    _ => Val::Null,
                },
            }
        }
        Expr::IsNull(inner, negated) => Val::Bool((eval(inner, message) == Val::Null) != *negated),
        Expr::In(inner, items, negated) => {
            let value = eval(inner, message);
            if value == Val::Null {
                return Val::Null;
            }
            let found = items.iter().any(|item| compare(&value, &eval(item, message)) == Some(std::cmp::Ordering::Equal));
            Val::Bool(found != *negated)
        }
        Expr::Like(inner, pattern, escape, negated) => match eval(inner, message) {
            Val::Str(text) => Val::Bool(like(&text, pattern, *escape) != *negated),
            _ => Val::Null,
        },
    }
}

#[cfg(test)]
mod tests {
    use fe2o3_amqp_types::messaging::{ApplicationProperties, Body, Message, Properties};
    use fe2o3_amqp_types::primitives::{OrderedMap, Value};

    use super::*;

    fn message() -> AmqpMessage {
        let mut application = OrderedMap::new();
        application.insert("color".to_string(), SimpleValue::String("red".into()));
        application.insert("quantity".to_string(), SimpleValue::Int(12));
        application.insert("price".to_string(), SimpleValue::Double(2.5.into()));
        application.insert("rush".to_string(), SimpleValue::Bool(true));
        Message {
            header: None,
            delivery_annotations: None,
            message_annotations: None,
            properties: Some(Properties { subject: Some("order".into()), ..Default::default() }),
            application_properties: Some(ApplicationProperties(application)),
            body: Body::Value(fe2o3_amqp_types::messaging::AmqpValue(Value::Null)),
            footer: None,
        }
    }

    fn check(expression: &str) -> bool {
        Filter::parse(expression).unwrap_or_else(|e| panic!("{expression}: {e}")).matches(&message())
    }

    #[test]
    fn filters() {
        for yes in [
            "1=1",
            "color = 'red'",
            "Color = 'red'".to_lowercase().as_str(),
            "user.color = 'red' AND quantity > 10",
            "quantity >= 12 and price < 3",
            "price * 4 = 10",
            "quantity % 5 = 2",
            "sys.Label = 'order'",
            "[color] <> 'blue'",
            "color IN ('green', 'red')",
            "color NOT IN ('green')",
            "color LIKE 'r%'",
            "color LIKE '_e_'",
            "color NOT LIKE 'b%'",
            "missing IS NULL",
            "color IS NOT NULL",
            "EXISTS(color) AND NOT EXISTS(missing)",
            "rush = TRUE",
            "rush",
            "(color = 'blue' OR quantity = 12) AND -quantity < 0",
            "missing = 1 OR color = 'red'",
            "color + 'dish' = 'reddish'",
        ] {
            assert!(check(yes), "{yes}");
        }
        for no in [
            "1=0",
            "color = 'blue'",
            "missing = 1",
            "NOT (missing = 1)",
            "missing <> 1",
            "color LIKE 'b%'",
            "color = 12",
            "quantity > 'a'",
        ] {
            assert!(!check(no), "{no}");
        }
    }

    #[test]
    fn like_escapes() {
        assert!(like("50%", "50!%", Some('!')));
        assert!(!like("500", "50!%", Some('!')));
        assert!(like("", "%", None));
    }

    #[test]
    fn bad_filters_are_refused() {
        for bad in ["color =", "color = 'red", "AND", "color = 'red' extra", "x IN 1", "x LIKE 5", "1 = (2"] {
            assert!(Filter::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn actions() {
        let action = Action::parse("SET priority = 'high'; SET quantity = quantity + 1; REMOVE color; SET sys.Label = 'urgent'").unwrap();
        let mut m = message();
        action.apply(&mut m);
        let props = &m.application_properties.as_ref().unwrap().0;
        assert_eq!(props.get("priority"), Some(&SimpleValue::String("high".into())));
        assert_eq!(props.get("quantity"), Some(&SimpleValue::Long(13)));
        assert!(props.get("color").is_none());
        assert_eq!(m.properties.unwrap().subject.as_deref(), Some("urgent"));
        assert!(Action::parse("SET sys.EnqueuedTimeUtc = 1").is_err());
        assert!(Action::parse("DROP x").is_err());
        assert!(Action::parse("").is_err());
    }
}
