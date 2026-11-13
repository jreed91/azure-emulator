//! The template expression language: `[concat(parameters('a'), '-x')]`.
//!
//! A JSON string is an expression when it starts with `[` and ends with `]`; a
//! string starting with `[[` is a literal with the first bracket removed. Inside,
//! an expression is a function call, a string in single quotes (`''` escapes a
//! quote), an integer, or one of those followed by member access (`.name`) and
//! indexing (`[0]`, `['name']`).

/// A parsed expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Str(String),
    Int(i64),
    /// A call by name. User-defined functions keep their namespace:
    /// `__bicep.myFunction`.
    Call(String, Vec<Expr>),
    Member(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
}

/// What a template string holds.
#[derive(Clone, Debug, PartialEq)]
pub enum TemplateString {
    Literal(String),
    Expression(Expr),
}

/// Classifies and parses one template string.
pub fn parse_string(text: &str) -> Result<TemplateString, String> {
    if text.starts_with("[[") {
        return Ok(TemplateString::Literal(text[1..].to_string()));
    }
    if text.len() >= 2 && text.starts_with('[') && text.ends_with(']') {
        let inner = &text[1..text.len() - 1];
        return parse(inner)
            .map(TemplateString::Expression)
            .map_err(|e| format!("Unable to parse language expression '{inner}': {e}"));
    }
    Ok(TemplateString::Literal(text.to_string()))
}

/// Parses the inside of an expression (without the outer brackets).
pub fn parse(text: &str) -> Result<Expr, String> {
    let tokens = lex(text)?;
    let mut parser = Parser { tokens, at: 0 };
    let expr = parser.expression()?;
    if parser.at != parser.tokens.len() {
        return Err(format!(
            "The template language expression '{text}' is not valid: unexpected '{}'.",
            parser.tokens[parser.at].describe()
        ));
    }
    Ok(expr)
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Question,
    Str(String),
    Int(i64),
    Ident(String),
}

impl Token {
    fn describe(&self) -> String {
        match self {
            Token::LParen => "(".into(),
            Token::RParen => ")".into(),
            Token::LBracket => "[".into(),
            Token::RBracket => "]".into(),
            Token::Comma => ",".into(),
            Token::Dot => ".".into(),
            Token::Question => "?".into(),
            Token::Str(s) => format!("'{s}'"),
            Token::Int(i) => i.to_string(),
            Token::Ident(s) => s.clone(),
        }
    }
}

fn lex(text: &str) -> Result<Vec<Token>, String> {
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
            '[' => {
                tokens.push(Token::LBracket);
                i += 1
            }
            ']' => {
                tokens.push(Token::RBracket);
                i += 1
            }
            ',' => {
                tokens.push(Token::Comma);
                i += 1
            }
            '.' => {
                tokens.push(Token::Dot);
                i += 1
            }
            '?' => {
                tokens.push(Token::Question);
                i += 1
            }
            '\'' => {
                let mut value = String::new();
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return Err(format!("The template language expression '{text}' has an unterminated string.")),
                        Some('\'') if chars.get(i + 1) == Some(&'\'') => {
                            value.push('\'');
                            i += 2;
                        }
                        Some('\'') => {
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
            }
            c if c.is_ascii_digit() || (c == '-' && chars.get(i + 1).is_some_and(char::is_ascii_digit)) => {
                let start = i;
                i += 1;
                while chars.get(i).is_some_and(char::is_ascii_digit) {
                    i += 1;
                }
                let digits: String = chars[start..i].iter().collect();
                let value = digits
                    .parse()
                    .map_err(|_| format!("The integer '{digits}' in expression '{text}' is out of range."))?;
                tokens.push(Token::Int(value));
            }
            c if c.is_alphabetic() || c == '_' || c == '$' => {
                let start = i;
                while chars.get(i).is_some_and(|c| c.is_alphanumeric() || *c == '_' || *c == '$') {
                    i += 1;
                }
                tokens.push(Token::Ident(chars[start..i].iter().collect()));
            }
            other => {
                return Err(format!(
                    "The template language expression '{text}' is not valid: the character '{other}' is not expected."
                ))
            }
        }
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn peek_at(&self, offset: usize) -> Option<&Token> {
        self.tokens.get(self.at + offset)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at).cloned();
        self.at += 1;
        token
    }

    fn expect(&mut self, wanted: Token) -> Result<(), String> {
        match self.next() {
            Some(token) if token == wanted => Ok(()),
            Some(token) => Err(format!("Expected '{}' but found '{}'.", wanted.describe(), token.describe())),
            None => Err(format!("Expected '{}' at the end of the expression.", wanted.describe())),
        }
    }

    fn expression(&mut self) -> Result<Expr, String> {
        let mut expr = self.primary()?;
        loop {
            match self.peek() {
                // `?.name` and `?[index]` are safe access: `tryGet`, null when missing.
                Some(Token::Question) if matches!(self.peek_at(1), Some(Token::Dot) | Some(Token::LBracket)) => {
                    self.at += 1;
                    let key = match self.next() {
                        Some(Token::Dot) => match self.next() {
                            Some(Token::Ident(name)) => Expr::Str(name),
                            _ => return Err("Expected a property name after '?.'.".into()),
                        },
                        _ => {
                            let index = self.expression()?;
                            self.expect(Token::RBracket)?;
                            index
                        }
                    };
                    expr = Expr::Call("tryGet".into(), vec![expr, key]);
                }
                Some(Token::Dot) => {
                    self.at += 1;
                    match self.next() {
                        Some(Token::Ident(name)) => expr = Expr::Member(Box::new(expr), name),
                        _ => return Err("Expected a property name after '.'.".into()),
                    }
                }
                Some(Token::LBracket) => {
                    self.at += 1;
                    let index = self.expression()?;
                    self.expect(Token::RBracket)?;
                    expr = Expr::Index(Box::new(expr), Box::new(index));
                }
                _ => return Ok(expr),
            }
        }
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.next() {
            Some(Token::Str(s)) => Ok(Expr::Str(s)),
            Some(Token::Int(i)) => Ok(Expr::Int(i)),
            Some(Token::Ident(mut name)) => {
                // A namespaced user-defined function: `ns.name(...)`.
                if self.peek() == Some(&Token::Dot)
                    && matches!(self.peek_at(1), Some(Token::Ident(_)))
                    && self.peek_at(2) == Some(&Token::LParen)
                {
                    self.at += 1;
                    if let Some(Token::Ident(member)) = self.next() {
                        name = format!("{name}.{member}");
                    }
                }
                if self.peek() != Some(&Token::LParen) {
                    // Bare `true`, `false` and `null` are accepted as constants.
                    return match name.to_ascii_lowercase().as_str() {
                        "true" | "false" | "null" => Ok(Expr::Call(name, Vec::new())),
                        _ => Err(format!("The template function '{name}' must be called with parentheses.")),
                    };
                }
                self.at += 1;
                let mut args = Vec::new();
                if self.peek() == Some(&Token::RParen) {
                    self.at += 1;
                    return Ok(Expr::Call(name, args));
                }
                loop {
                    args.push(self.expression()?);
                    match self.next() {
                        Some(Token::Comma) => continue,
                        Some(Token::RParen) => break,
                        _ => return Err(format!("Expected ',' or ')' in the arguments of '{name}'.")),
                    }
                }
                Ok(Expr::Call(name, args))
            }
            Some(token) => Err(format!("Unexpected '{}' in the expression.", token.describe())),
            None => Err("The expression is empty.".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: Vec<Expr>) -> Expr {
        Expr::Call(name.into(), args)
    }

    #[test]
    fn classifies_strings() {
        assert_eq!(parse_string("plain").unwrap(), TemplateString::Literal("plain".into()));
        assert_eq!(parse_string("[[not]").unwrap(), TemplateString::Literal("[not]".into()));
        assert_eq!(
            parse_string("[parameters('a')]").unwrap(),
            TemplateString::Expression(call("parameters", vec![Expr::Str("a".into())]))
        );
    }

    #[test]
    fn parses_access_and_escapes() {
        let expr = parse("reference('x').outputs['y'].value[0]").unwrap();
        let expected = Expr::Index(
            Box::new(Expr::Member(
                Box::new(Expr::Index(
                    Box::new(Expr::Member(
                        Box::new(call("reference", vec![Expr::Str("x".into())])),
                        "outputs".into(),
                    )),
                    Box::new(Expr::Str("y".into())),
                )),
                "value".into(),
            )),
            Box::new(Expr::Int(0)),
        );
        assert_eq!(expr, expected);
        assert_eq!(parse("'it''s'").unwrap(), Expr::Str("it's".into()));
        assert_eq!(parse("add(-1, 2)").unwrap(), call("add", vec![Expr::Int(-1), Expr::Int(2)]));
        assert_eq!(parse("__bicep.f(1)").unwrap(), call("__bicep.f", vec![Expr::Int(1)]));
        assert_eq!(
            parse("a()?.b").unwrap(),
            call("tryGet", vec![call("a", vec![]), Expr::Str("b".into())])
        );
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse("concat('a'").is_err());
        assert!(parse("'open").is_err());
        assert!(parse("foo").is_err());
        assert!(parse("a() b()").is_err());
    }
}
