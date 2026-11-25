//! Splits T-SQL text into tokens.

use crate::exec::SqlError;

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    /// A word: a keyword or a plain identifier (kept as written).
    Word(String),
    /// A `[delimited]` or `"quoted"` identifier, never a keyword.
    Quoted(String),
    /// `@name` (with the `@`), or `@@name`.
    Var(String),
    Int(String),
    Number(String),
    Str(String),
    NStr(String),
    Binary(Vec<u8>),
    Sym(&'static str),
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    /// Line, from 1.
    pub line: u32,
    /// Byte offset of the token's start in the text.
    pub start: usize,
    pub end: usize,
}

const SYMBOLS: [&str; 27] = [
    "<>", "!=", "<=", ">=", "!<", "!>", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "::", "=", "<", ">", "(", ")", ",", ";", ".", "+", "-", "*", "/",
];
const SINGLE: [&str; 6] = ["%", "&", "|", "^", "~", ":"];

fn syntax_error(near: &str, line: u32) -> SqlError {
    let mut error = SqlError::new(102, 15, format!("Incorrect syntax near '{near}'."));
    error.line = line;
    error
}

pub fn tokenize(text: &str) -> Result<Vec<Token>, SqlError> {
    let bytes = text.as_bytes();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    let mut line = 1u32;
    let at = |i: usize| chars.get(i).map(|c| c.1);
    let offset = |i: usize| chars.get(i).map_or(bytes.len(), |c| c.0);
    while i < chars.len() {
        let c = chars[i].1;
        let start = offset(i);
        let start_line = line;
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // Comments.
        if c == '-' && at(i + 1) == Some('-') {
            while i < chars.len() && chars[i].1 != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && at(i + 1) == Some('*') {
            let mut depth = 0;
            loop {
                match (at(i), at(i + 1)) {
                    (Some('/'), Some('*')) => {
                        depth += 1;
                        i += 2;
                    }
                    (Some('*'), Some('/')) => {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    }
                    (Some('\n'), _) => {
                        line += 1;
                        i += 1;
                    }
                    (Some(_), _) => i += 1,
                    (None, _) => {
                        let mut error = SqlError::new(113, 15, "Missing end comment mark '*/'.");
                        error.line = start_line;
                        return Err(error);
                    }
                }
            }
            continue;
        }
        let tok;
        // Strings: 'text' and N'text'.
        if c == '\'' || ((c == 'N' || c == 'n') && at(i + 1) == Some('\'')) {
            let national = c != '\'';
            i += if national { 2 } else { 1 };
            let mut s = String::new();
            loop {
                match at(i) {
                    Some('\'') if at(i + 1) == Some('\'') => {
                        s.push('\'');
                        i += 2;
                    }
                    Some('\'') => {
                        i += 1;
                        break;
                    }
                    Some(ch) => {
                        if ch == '\n' {
                            line += 1;
                        }
                        s.push(ch);
                        i += 1;
                    }
                    None => {
                        let mut error = SqlError::new(105, 15, format!("Unclosed quotation mark after the character string '{s}'."));
                        error.line = start_line;
                        return Err(error);
                    }
                }
            }
            tok = if national { Tok::NStr(s) } else { Tok::Str(s) };
        } else if c == '[' {
            i += 1;
            let mut s = String::new();
            loop {
                match at(i) {
                    Some(']') if at(i + 1) == Some(']') => {
                        s.push(']');
                        i += 2;
                    }
                    Some(']') => {
                        i += 1;
                        break;
                    }
                    Some(ch) => {
                        s.push(ch);
                        i += 1;
                    }
                    None => {
                        let mut error = SqlError::new(105, 15, format!("Unclosed quotation mark after the character string '{s}'."));
                        error.line = start_line;
                        return Err(error);
                    }
                }
            }
            tok = Tok::Quoted(s);
        } else if c == '"' {
            i += 1;
            let mut s = String::new();
            loop {
                match at(i) {
                    Some('"') if at(i + 1) == Some('"') => {
                        s.push('"');
                        i += 2;
                    }
                    Some('"') => {
                        i += 1;
                        break;
                    }
                    Some(ch) => {
                        s.push(ch);
                        i += 1;
                    }
                    None => return Err(syntax_error(&s, start_line)),
                }
            }
            tok = Tok::Quoted(s);
        } else if c == '0' && matches!(at(i + 1), Some('x' | 'X')) {
            i += 2;
            let mut hex = String::new();
            while let Some(ch) = at(i).filter(char::is_ascii_hexdigit) {
                hex.push(ch);
                i += 1;
            }
            if hex.len() % 2 == 1 {
                hex.insert(0, '0');
            }
            let bytes = (0..hex.len()).step_by(2).map(|j| u8::from_str_radix(&hex[j..j + 2], 16).unwrap_or(0)).collect();
            tok = Tok::Binary(bytes);
        } else if c.is_ascii_digit() || (c == '.' && at(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let mut s = String::new();
            let mut decimal = false;
            while let Some(ch) = at(i) {
                if ch.is_ascii_digit() {
                    s.push(ch);
                } else if ch == '.' && !decimal {
                    decimal = true;
                    s.push(ch);
                } else if (ch == 'e' || ch == 'E') && at(i + 1).is_some_and(|d| d.is_ascii_digit() || d == '-' || d == '+') {
                    decimal = true;
                    s.push(ch);
                    i += 1;
                    if let Some(sign) = at(i).filter(|d| *d == '-' || *d == '+') {
                        s.push(sign);
                        i += 1;
                    }
                    continue;
                } else {
                    break;
                }
                i += 1;
            }
            tok = if decimal { Tok::Number(s) } else { Tok::Int(s) };
        } else if c == '@' {
            let mut s = String::from("@");
            i += 1;
            if at(i) == Some('@') {
                s.push('@');
                i += 1;
            }
            while let Some(ch) = at(i).filter(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '@' | '#' | '$')) {
                s.push(ch);
                i += 1;
            }
            tok = Tok::Var(s);
        } else if c.is_alphabetic() || c == '_' || c == '#' {
            let mut s = String::new();
            while let Some(ch) = at(i).filter(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '@' | '#' | '$')) {
                s.push(ch);
                i += 1;
            }
            tok = Tok::Word(s);
        } else {
            let rest = &text[start..];
            if let Some(symbol) = SYMBOLS.iter().chain(SINGLE.iter()).find(|s| rest.starts_with(**s)) {
                i += symbol.chars().count();
                tok = Tok::Sym(symbol);
            } else {
                return Err(syntax_error(&c.to_string(), line));
            }
        }
        tokens.push(Token { tok, line: start_line, start, end: offset(i) });
    }
    tokens.push(Token { tok: Tok::Eof, line, start: bytes.len(), end: bytes.len() });
    Ok(tokens)
}
