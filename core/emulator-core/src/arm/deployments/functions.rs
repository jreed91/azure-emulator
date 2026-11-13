//! Evaluation of template expressions and the built-in template functions.
//!
//! <https://learn.microsoft.com/azure/azure-resource-manager/templates/template-functions>

use std::rc::Rc;

use base64::Engine as _;
use serde_json::{json, Map, Value};

use super::expr::{self, Expr, TemplateString};
use super::template::Ctx;
use crate::ArmError;

/// Values bound while evaluating part of a template: loop indexes, lambda
/// variables and the arguments of a user-defined function.
#[derive(Clone, Default)]
pub struct Locals {
    /// Copy loops by name, innermost last.
    pub copies: Vec<(String, i64)>,
    pub lambda: Vec<(String, Value)>,
    pub function_args: Option<Rc<Map<String, Value>>>,
}

impl Locals {
    pub fn with_copy(&self, name: &str, index: i64) -> Self {
        let mut out = self.clone();
        out.copies.push((name.to_string(), index));
        out
    }

    fn with_lambda(&self, bindings: Vec<(String, Value)>) -> Self {
        let mut out = self.clone();
        out.lambda.extend(bindings);
        out
    }
}

/// The error for an expression that cannot be evaluated.
pub fn invalid(message: impl Into<String>) -> ArmError {
    ArmError::bad_request("InvalidTemplate", message)
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "Null",
        Value::Bool(_) => "Boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "Integer",
        Value::Number(_) => "Float",
        Value::String(_) => "String",
        Value::Array(_) => "Array",
        Value::Object(_) => "Object",
    }
}

/// `string()`: how a value reads as text.
pub fn to_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => String::new(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

fn int(value: &Value, function: &str) -> Result<i64, ArmError> {
    value.as_i64().ok_or_else(|| {
        invalid(format!(
            "The template function '{function}' expects an integer, but got a value of type '{}'.",
            type_name(value)
        ))
    })
}

fn string<'v>(value: &'v Value, function: &str) -> Result<&'v str, ArmError> {
    value.as_str().ok_or_else(|| {
        invalid(format!(
            "The template function '{function}' expects a string, but got a value of type '{}'.",
            type_name(value)
        ))
    })
}

fn array<'v>(value: &'v Value, function: &str) -> Result<&'v Vec<Value>, ArmError> {
    value.as_array().ok_or_else(|| {
        invalid(format!(
            "The template function '{function}' expects an array, but got a value of type '{}'.",
            type_name(value)
        ))
    })
}

fn object<'v>(value: &'v Value, function: &str) -> Result<&'v Map<String, Value>, ArmError> {
    value.as_object().ok_or_else(|| {
        invalid(format!(
            "The template function '{function}' expects an object, but got a value of type '{}'.",
            type_name(value)
        ))
    })
}

fn boolean(value: &Value, function: &str) -> Result<bool, ArmError> {
    value.as_bool().ok_or_else(|| {
        invalid(format!(
            "The template function '{function}' expects a boolean, but got a value of type '{}'.",
            type_name(value)
        ))
    })
}

/// A member of an object; template property access ignores case.
pub fn member<'v>(object: &'v Map<String, Value>, name: &str) -> Option<&'v Value> {
    object
        .get(name)
        .or_else(|| object.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v))
}

fn arity(name: &str, args: &[Expr], min: usize, max: usize) -> Result<(), ArmError> {
    if args.len() < min || args.len() > max {
        let expected = if min == max {
            format!("{min}")
        } else if max == usize::MAX {
            format!("at least {min}")
        } else {
            format!("{min} to {max}")
        };
        return Err(invalid(format!(
            "The template function '{name}' expects {expected} parameter(s), but {} were given.",
            args.len()
        )));
    }
    Ok(())
}

/// The parameter names and body of a `lambda(...)` argument.
fn lambda<'e>(expr: &'e Expr, function: &str) -> Result<(Vec<String>, &'e Expr), ArmError> {
    if let Expr::Call(name, args) = expr {
        if name.eq_ignore_ascii_case("lambda") && !args.is_empty() {
            let (body, names) = args.split_last().expect("not empty");
            let names = names
                .iter()
                .map(|n| match n {
                    Expr::Str(s) => Ok(s.clone()),
                    _ => Err(invalid("Lambda parameter names must be string literals.")),
                })
                .collect::<Result<_, _>>()?;
            return Ok((names, body));
        }
    }
    Err(invalid(format!("The template function '{function}' expects a lambda expression.")))
}

fn compare(a: &Value, b: &Value, function: &str) -> Result<std::cmp::Ordering, ArmError> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap_or(0.0), y.as_f64().unwrap_or(0.0));
            Ok(x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal))
        }
        (Value::String(x), Value::String(y)) => Ok(x.cmp(y)),
        _ => Err(invalid(format!(
            "The template function '{function}' expects two integers or two strings, but got '{}' and '{}'.",
            type_name(a),
            type_name(b)
        ))),
    }
}

/// `union` merges objects recursively and arrays without duplicates.
fn union_values(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let mut out = x.clone();
            for (key, value) in y {
                let merged = match out.get(key) {
                    Some(existing @ Value::Object(_)) if value.is_object() => union_values(existing, value),
                    _ => value.clone(),
                };
                out.insert(key.clone(), merged);
            }
            Value::Object(out)
        }
        (Value::Array(x), Value::Array(y)) => {
            let mut out: Vec<Value> = Vec::new();
            for item in x.iter().chain(y) {
                if !out.contains(item) {
                    out.push(item.clone());
                }
            }
            Value::Array(out)
        }
        (_, b) => b.clone(),
    }
}

/// .NET composite formatting as `format()` uses it: `{0}`, `{1:N0}`, `{{`.
fn composite_format(pattern: &str, args: &[Value]) -> Result<String, ArmError> {
    let mut out = String::new();
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '{' if chars.get(i + 1) == Some(&'{') => {
                out.push('{');
                i += 2;
            }
            '}' if chars.get(i + 1) == Some(&'}') => {
                out.push('}');
                i += 2;
            }
            '{' => {
                let end = chars[i..]
                    .iter()
                    .position(|c| *c == '}')
                    .ok_or_else(|| invalid(format!("The format string '{pattern}' is not valid.")))?;
                let spec: String = chars[i + 1..i + end].iter().collect();
                let (head, format) = spec.split_once(':').unwrap_or((&spec, ""));
                let (index, _alignment) = head.split_once(',').unwrap_or((head, ""));
                let index: usize = index
                    .trim()
                    .parse()
                    .map_err(|_| invalid(format!("The format string '{pattern}' is not valid.")))?;
                let value = args.get(index).ok_or_else(|| {
                    invalid(format!(
                        "The format string '{pattern}' refers to argument {index}, but only {} were given.",
                        args.len()
                    ))
                })?;
                out.push_str(&format_value(value, format));
                i += end + 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

fn format_value(value: &Value, format: &str) -> String {
    let Some(number) = value.as_i64() else {
        return to_text(value);
    };
    let (kind, digits) = format.split_at(format.len().min(1));
    let digits: usize = digits.parse().unwrap_or(0);
    match kind {
        "N" | "n" => {
            let text = number.unsigned_abs().to_string();
            let mut grouped = String::new();
            for (i, c) in text.chars().enumerate() {
                if i > 0 && (text.len() - i) % 3 == 0 {
                    grouped.push(',');
                }
                grouped.push(c);
            }
            let sign = if number < 0 { "-" } else { "" };
            match digits {
                0 => format!("{sign}{grouped}"),
                d => format!("{sign}{grouped}.{}", "0".repeat(d)),
            }
        }
        "D" | "d" => {
            let sign = if number < 0 { "-" } else { "" };
            format!("{sign}{:0>digits$}", number.unsigned_abs())
        }
        "X" => format!("{number:0>digits$X}"),
        "x" => format!("{number:0>digits$x}"),
        _ => number.to_string(),
    }
}

/// `uriComponent`: .NET `Uri.EscapeDataString`.
fn escape_data(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// `uri(base, relative)`: RFC 3986 reference resolution for the common cases.
fn resolve_uri(base: &str, relative: &str) -> String {
    if relative.contains("://") {
        return relative.to_string();
    }
    let origin_end = base.find("://").map(|i| i + 3).and_then(|start| {
        base[start..].find('/').map(|slash| start + slash).or(Some(base.len()))
    });
    match origin_end {
        Some(end) if relative.starts_with('/') => format!("{}{relative}", &base[..end]),
        Some(end) => {
            let path_end = base.rfind('/').filter(|i| *i >= end).map(|i| i + 1);
            match path_end {
                Some(cut) => format!("{}{relative}", &base[..cut]),
                None => format!("{}/{relative}", &base[..end]),
            }
        }
        None => format!("{base}{relative}"),
    }
}

/// ARM's `guid()`: a version 5 UUID of the arguments joined with `-`.
pub fn template_guid(parts: &[String]) -> String {
    let namespace = uuid::Uuid::from_u128(0x11fb06fb_712d_4ddd_98c7_e71bbd588830);
    uuid::Uuid::new_v5(&namespace, parts.join("-").as_bytes()).to_string()
}

/// ARM's `uniqueString()`: a 64-bit Murmur hash of the arguments joined with `-`,
/// as 13 base-32 characters.
pub fn unique_string(parts: &[String]) -> String {
    let mut hash = murmur64(parts.join("-").as_bytes());
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity(13);
    for _ in 0..13 {
        out.push(ALPHABET[(hash >> 59) as usize] as char);
        hash <<= 5;
    }
    out
}

/// The 64-bit variant of MurmurHash3 that Resource Manager uses.
pub fn murmur64(data: &[u8]) -> u64 {
    const C1: u32 = 0x239b961b;
    const C2: u32 = 0xab0e9789;
    let word = |bytes: &[u8]| -> u32 {
        bytes.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (8 * i)))
    };
    let (mut h1, mut h2) = (0u32, 0u32);
    let (chunks, tail) = data.as_chunks::<8>();
    for chunk in chunks {
        let k1 = word(&chunk[..4]).wrapping_mul(C1).rotate_left(15).wrapping_mul(C2);
        h1 ^= k1;
        h1 = h1.rotate_left(19).wrapping_add(h2).wrapping_mul(5).wrapping_add(0x561ccd1b);
        let k2 = word(&chunk[4..]).wrapping_mul(C2).rotate_left(17).wrapping_mul(C1);
        h2 ^= k2;
        h2 = h2.rotate_left(13).wrapping_add(h1).wrapping_mul(5).wrapping_add(0x0bcaa747);
    }
    if !tail.is_empty() {
        let k1 = word(&tail[..tail.len().min(4)]).wrapping_mul(C1).rotate_left(15).wrapping_mul(C2);
        h1 ^= k1;
        if tail.len() > 4 {
            let k2 = word(&tail[4..]).wrapping_mul(C2).rotate_left(17).wrapping_mul(C1);
            h2 ^= k2;
        }
    }
    let length = data.len() as u32;
    h1 ^= length;
    h2 ^= length;
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    h1 = fmix(h1);
    h2 = fmix(h2);
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    (u64::from(h2) << 32) | u64::from(h1)
}

fn fmix(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x85ebca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2ae35);
    h ^= h >> 16;
    h
}

/// .NET custom date and time format strings, for `utcNow` and `dateTimeAdd`.
fn format_time(time: &chrono::DateTime<chrono::Utc>, format: &str) -> String {
    use chrono::{Datelike, Timelike};
    match format {
        "o" | "O" => return super::template::timestamp(time),
        "u" => return time.format("%Y-%m-%d %H:%M:%SZ").to_string(),
        "s" => return time.format("%Y-%m-%dT%H:%M:%S").to_string(),
        "d" => return time.format("%-m/%-d/%Y").to_string(),
        _ => {}
    }
    let chars: Vec<char> = format.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let run = chars[i..].iter().take_while(|x| **x == c).count();
        let text = match c {
            'y' if run >= 4 => format!("{:04}", time.year()),
            'y' => format!("{:02}", time.year() % 100),
            'M' if run >= 2 => format!("{:02}", time.month()),
            'M' => time.month().to_string(),
            'd' if run >= 2 => format!("{:02}", time.day()),
            'd' => time.day().to_string(),
            'H' if run >= 2 => format!("{:02}", time.hour()),
            'H' => time.hour().to_string(),
            'h' => format!("{:02}", (time.hour() + 11) % 12 + 1),
            'm' if run >= 2 => format!("{:02}", time.minute()),
            'm' => time.minute().to_string(),
            's' if run >= 2 => format!("{:02}", time.second()),
            's' => time.second().to_string(),
            'f' | 'F' => {
                let nanos = format!("{:09}", time.nanosecond());
                nanos[..run.min(9)].to_string()
            }
            't' => (if time.hour() < 12 { "AM" } else { "PM" })[..run.min(2)].to_string(),
            '\\' if i + 1 < chars.len() => {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            '\'' | '"' => {
                let end = chars[i + 1..].iter().position(|x| *x == c).map(|p| i + 1 + p);
                let end = end.unwrap_or(chars.len());
                out.extend(&chars[i + 1..end]);
                i = end + 1;
                continue;
            }
            other => {
                out.push(other);
                i += 1;
                continue;
            }
        };
        out.push_str(&text);
        i += run;
    }
    out
}

fn parse_time(text: &str) -> Result<chrono::DateTime<chrono::Utc>, ArmError> {
    chrono::DateTime::parse_from_rfc3339(text)
        .map(|t| t.with_timezone(&chrono::Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f").map(|t| t.and_utc())
        })
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .map(|d| d.and_hms_opt(0, 0, 0).expect("midnight exists").and_utc())
        })
        .map_err(|_| invalid(format!("The value '{text}' is not a valid date and time.")))
}

/// Adds an ISO 8601 duration (`P1D`, `-PT1H30M`, `P1Y2M`) to a time.
fn add_duration(
    time: chrono::DateTime<chrono::Utc>,
    duration: &str,
) -> Result<chrono::DateTime<chrono::Utc>, ArmError> {
    let bad = || invalid(format!("The duration '{duration}' is not a valid ISO 8601 duration."));
    let (negative, rest) = match duration.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, duration),
    };
    let rest = rest.strip_prefix('P').ok_or_else(bad)?;
    let mut months = 0i64;
    let mut seconds = 0f64;
    let mut in_time = false;
    let mut number = String::new();
    for c in rest.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' | '.' => number.push(c),
            unit => {
                let value: f64 = number.parse().map_err(|_| bad())?;
                number.clear();
                match (in_time, unit) {
                    (false, 'Y') => months += (value as i64) * 12,
                    (false, 'M') => months += value as i64,
                    (false, 'W') => seconds += value * 7.0 * 86400.0,
                    (false, 'D') => seconds += value * 86400.0,
                    (true, 'H') => seconds += value * 3600.0,
                    (true, 'M') => seconds += value * 60.0,
                    (true, 'S') => seconds += value,
                    _ => return Err(bad()),
                }
            }
        }
    }
    if !number.is_empty() {
        return Err(bad());
    }
    let sign = if negative { -1 } else { 1 };
    let mut out = time;
    if months != 0 {
        let shifted = if sign * months > 0 {
            out.checked_add_months(chrono::Months::new((months) as u32))
        } else {
            out.checked_sub_months(chrono::Months::new((months) as u32))
        };
        out = shifted.ok_or_else(bad)?;
    }
    let millis = (seconds * 1000.0).round() as i64 * sign;
    Ok(out + chrono::Duration::milliseconds(millis))
}

impl Ctx<'_> {
    /// Evaluates a JSON value from the template: expressions in strings are
    /// evaluated, and `copy` arrays inside objects expand into properties.
    pub fn eval_value(&self, value: &Value, locals: &Locals) -> Result<Value, ArmError> {
        match value {
            Value::String(text) => self.eval_string(text, locals),
            Value::Array(items) => items
                .iter()
                .map(|v| self.eval_value(v, locals))
                .collect::<Result<_, _>>()
                .map(Value::Array),
            Value::Object(map) => {
                let mut out = Map::new();
                for (key, item) in map {
                    if key == "copy" {
                        if let Some(loops) = property_copies(item) {
                            for (name, count, input) in loops {
                                let count = int(&self.eval_value(count, locals)?, "copy")?;
                                let mut values = Vec::new();
                                for index in 0..count.max(0) {
                                    values.push(self.eval_value(input, &locals.with_copy(name, index))?);
                                }
                                out.insert(name.to_string(), Value::Array(values));
                            }
                            continue;
                        }
                    }
                    out.insert(key.clone(), self.eval_value(item, locals)?);
                }
                Ok(Value::Object(out))
            }
            other => Ok(other.clone()),
        }
    }

    /// Evaluates one template string.
    pub fn eval_string(&self, text: &str, locals: &Locals) -> Result<Value, ArmError> {
        match expr::parse_string(text).map_err(invalid)? {
            TemplateString::Literal(s) => Ok(Value::String(s)),
            TemplateString::Expression(e) => self.eval(&e, locals),
        }
    }

    pub fn eval(&self, expr: &Expr, locals: &Locals) -> Result<Value, ArmError> {
        match expr {
            Expr::Str(s) => Ok(Value::String(s.clone())),
            Expr::Int(i) => Ok(json!(i)),
            Expr::Member(target, name) => {
                let value = self.eval(target, locals)?;
                let object = value.as_object().ok_or_else(|| {
                    invalid(format!(
                        "The language expression property '{name}' can't be evaluated, property name must be a string... the value is of type '{}'.",
                        type_name(&value)
                    ))
                })?;
                member(object, name).cloned().ok_or_else(|| {
                    let available: Vec<&str> = object.keys().map(String::as_str).collect();
                    invalid(format!(
                        "The language expression property '{name}' doesn't exist, available properties are '{}'.",
                        available.join(", ")
                    ))
                })
            }
            Expr::Index(target, index) => {
                let value = self.eval(target, locals)?;
                let index = self.eval(index, locals)?;
                match (&value, &index) {
                    (Value::Array(items), Value::Number(n)) => {
                        let i = n.as_i64().unwrap_or(-1);
                        usize::try_from(i).ok().and_then(|i| items.get(i)).cloned().ok_or_else(|| {
                            invalid(format!(
                                "The language expression property array index '{i}' is out of bounds."
                            ))
                        })
                    }
                    (Value::Object(map), Value::String(name)) => member(map, name).cloned().ok_or_else(|| {
                        invalid(format!("The language expression property '{name}' doesn't exist."))
                    }),
                    _ => Err(invalid(format!(
                        "A value of type '{}' cannot be indexed by a value of type '{}'.",
                        type_name(&value),
                        type_name(&index)
                    ))),
                }
            }
            Expr::Call(name, args) => self.call(name, args, locals),
        }
    }

    fn args(&self, args: &[Expr], locals: &Locals) -> Result<Vec<Value>, ArmError> {
        args.iter().map(|a| self.eval(a, locals)).collect()
    }

    fn call(&self, name: &str, args: &[Expr], locals: &Locals) -> Result<Value, ArmError> {
        let lower = name.to_ascii_lowercase();
        if lower.contains('.') {
            return self.call_user_function(name, args, locals);
        }
        if lower.starts_with("list") {
            arity(name, args, 2, 3)?;
            let values = self.args(args, locals)?;
            let id = string(&values[0], name)?;
            let api_version = string(&values[1], name)?;
            return self.list_action(id, name, api_version, values.get(2));
        }
        // Functions that evaluate their arguments themselves.
        match lower.as_str() {
            "if" => {
                arity(name, args, 3, 3)?;
                let condition = boolean(&self.eval(&args[0], locals)?, name)?;
                return self.eval(&args[if condition { 1 } else { 2 }], locals);
            }
            "and" | "or" => {
                arity(name, args, 2, usize::MAX)?;
                let want = lower == "or";
                for arg in args {
                    if boolean(&self.eval(arg, locals)?, name)? == want {
                        return Ok(Value::Bool(want));
                    }
                }
                return Ok(Value::Bool(!want));
            }
            "coalesce" => {
                for arg in args {
                    let value = self.eval(arg, locals)?;
                    if !value.is_null() {
                        return Ok(value);
                    }
                }
                return Ok(Value::Null);
            }
            "lambda" => return Err(invalid("A lambda expression can only be an argument of a function that takes one.")),
            "lambdavariables" => {
                arity(name, args, 1, 1)?;
                let wanted = self.eval(&args[0], locals)?;
                let wanted = string(&wanted, name)?;
                return locals
                    .lambda
                    .iter()
                    .rev()
                    .find(|(n, _)| n == wanted)
                    .map(|(_, v)| v.clone())
                    .ok_or_else(|| invalid(format!("The lambda variable '{wanted}' is not defined.")));
            }
            "filter" | "map" | "sort" | "groupby" | "mapvalues" => {
                arity(name, args, 2, 2)?;
                let source = self.eval(&args[0], locals)?;
                let (params, body) = lambda(&args[1], name)?;
                return self.collection_lambda(&lower, name, &source, &params, body, locals);
            }
            "reduce" => {
                arity(name, args, 3, 3)?;
                let source = self.eval(&args[0], locals)?;
                let mut accumulator = self.eval(&args[1], locals)?;
                let (params, body) = lambda(&args[2], name)?;
                for (index, item) in array(&source, name)?.iter().enumerate() {
                    let mut bindings = vec![];
                    for (slot, value) in params.iter().zip([accumulator.clone(), item.clone(), json!(index)]) {
                        bindings.push((slot.clone(), value));
                    }
                    accumulator = self.eval(body, &locals.with_lambda(bindings))?;
                }
                return Ok(accumulator);
            }
            "toobject" => {
                arity(name, args, 2, 3)?;
                let source = self.eval(&args[0], locals)?;
                let (key_params, key_body) = lambda(&args[1], name)?;
                let value_lambda = args.get(2).map(|a| lambda(a, name)).transpose()?;
                let mut out = Map::new();
                for item in array(&source, name)? {
                    let bind = |params: &[String]| {
                        locals.with_lambda(params.iter().take(1).map(|p| (p.clone(), item.clone())).collect())
                    };
                    let key = self.eval(key_body, &bind(&key_params))?;
                    let value = match &value_lambda {
                        Some((params, body)) => self.eval(body, &bind(params))?,
                        None => item.clone(),
                    };
                    out.insert(to_text(&key), value);
                }
                return Ok(Value::Object(out));
            }
            _ => {}
        }
        let values = self.args(args, locals)?;
        self.builtin(name, &lower, &values, locals)
    }

    fn collection_lambda(
        &self,
        lower: &str,
        name: &str,
        source: &Value,
        params: &[String],
        body: &Expr,
        locals: &Locals,
    ) -> Result<Value, ArmError> {
        let bind = |values: Vec<Value>| locals.with_lambda(params.iter().cloned().zip(values).collect());
        if lower == "mapvalues" {
            let mut out = Map::new();
            for (key, value) in object(source, name)? {
                out.insert(key.clone(), self.eval(body, &bind(vec![value.clone()]))?);
            }
            return Ok(Value::Object(out));
        }
        let items = array(source, name)?;
        match lower {
            "filter" => {
                let mut out = Vec::new();
                for (index, item) in items.iter().enumerate() {
                    if boolean(&self.eval(body, &bind(vec![item.clone(), json!(index)]))?, name)? {
                        out.push(item.clone());
                    }
                }
                Ok(Value::Array(out))
            }
            "map" => items
                .iter()
                .enumerate()
                .map(|(index, item)| self.eval(body, &bind(vec![item.clone(), json!(index)])))
                .collect::<Result<_, _>>()
                .map(Value::Array),
            "groupby" => {
                let mut out = Map::new();
                for item in items {
                    let key = to_text(&self.eval(body, &bind(vec![item.clone()]))?);
                    let group = out.entry(key).or_insert_with(|| Value::Array(Vec::new()));
                    if let Value::Array(group) = group {
                        group.push(item.clone());
                    }
                }
                Ok(Value::Object(out))
            }
            _ => {
                // sort: the lambda says whether its first argument goes first.
                let mut sorted = items.clone();
                let mut error = None;
                sorted.sort_by(|a, b| {
                    if error.is_some() {
                        return std::cmp::Ordering::Equal;
                    }
                    let first = |x: &Value, y: &Value| {
                        self.eval(body, &bind(vec![x.clone(), y.clone()])).and_then(|v| boolean(&v, name))
                    };
                    match (first(a, b), first(b, a)) {
                        (Ok(true), _) => std::cmp::Ordering::Less,
                        (Ok(false), Ok(true)) => std::cmp::Ordering::Greater,
                        (Ok(false), Ok(false)) => std::cmp::Ordering::Equal,
                        (Err(e), _) | (_, Err(e)) => {
                            error = Some(e);
                            std::cmp::Ordering::Equal
                        }
                    }
                });
                match error {
                    Some(e) => Err(e),
                    None => Ok(Value::Array(sorted)),
                }
            }
        }
    }

    fn call_user_function(&self, name: &str, args: &[Expr], locals: &Locals) -> Result<Value, ArmError> {
        let definition = self
            .user_function(name)
            .ok_or_else(|| invalid(format!("The template function '{name}' is not valid.")))?;
        let params = definition.get("parameters").and_then(Value::as_array).cloned().unwrap_or_default();
        if params.len() != args.len() {
            return Err(invalid(format!(
                "The function '{name}' expects {} parameter(s), but {} were given.",
                params.len(),
                args.len()
            )));
        }
        let mut bound = Map::new();
        for (param, arg) in params.iter().zip(args) {
            let param_name = param.get("name").and_then(Value::as_str).unwrap_or_default();
            bound.insert(param_name.to_string(), self.eval(arg, locals)?);
        }
        let output = definition
            .pointer("/output/value")
            .ok_or_else(|| invalid(format!("The function '{name}' has no output value.")))?;
        // A user function sees only its own parameters.
        let inner = Locals {
            function_args: Some(Rc::new(bound)),
            ..Locals::default()
        };
        self.eval_value(output, &inner)
    }

    fn builtin(&self, name: &str, lower: &str, v: &[Value], locals: &Locals) -> Result<Value, ArmError> {
        let n = v.len();
        let need = |min: usize, max: usize| -> Result<(), ArmError> {
            if n < min || n > max {
                let expected = if min == max { format!("{min}") } else { format!("{min} to {max}") };
                return Err(invalid(format!(
                    "The template function '{name}' expects {expected} parameter(s), but {n} were given."
                )));
            }
            Ok(())
        };
        Ok(match lower {
            // ---- logical and comparison
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            "null" => Value::Null,
            "not" => {
                need(1, 1)?;
                Value::Bool(!boolean(&v[0], name)?)
            }
            "bool" => {
                need(1, 1)?;
                match &v[0] {
                    Value::Bool(b) => Value::Bool(*b),
                    Value::String(s) if s.eq_ignore_ascii_case("true") => Value::Bool(true),
                    Value::String(s) if s.eq_ignore_ascii_case("false") => Value::Bool(false),
                    Value::Number(x) => Value::Bool(x.as_i64() != Some(0)),
                    other => return Err(invalid(format!("The value '{}' cannot be converted to a boolean.", to_text(other)))),
                }
            }
            "equals" => {
                need(2, 2)?;
                Value::Bool(values_equal(&v[0], &v[1]))
            }
            "less" | "lessorequals" | "greater" | "greaterorequals" => {
                need(2, 2)?;
                use std::cmp::Ordering::*;
                let order = compare(&v[0], &v[1], name)?;
                Value::Bool(match lower {
                    "less" => order == Less,
                    "lessorequals" => order != Greater,
                    "greater" => order == Greater,
                    _ => order != Less,
                })
            }
            // ---- numeric
            "add" | "sub" | "mul" | "div" | "mod" => {
                need(2, 2)?;
                let (a, b) = (int(&v[0], name)?, int(&v[1], name)?);
                let result = match lower {
                    "add" => a.checked_add(b),
                    "sub" => a.checked_sub(b),
                    "mul" => a.checked_mul(b),
                    _ if b == 0 => return Err(invalid(format!("The template function '{name}' cannot divide by zero."))),
                    "div" => a.checked_div(b),
                    _ => a.checked_rem(b),
                };
                json!(result.ok_or_else(|| invalid(format!("The template function '{name}' overflowed.")))?)
            }
            "int" => {
                need(1, 1)?;
                match &v[0] {
                    Value::Number(x) => json!(x.as_i64().unwrap_or_else(|| x.as_f64().unwrap_or(0.0) as i64)),
                    Value::String(s) => json!(s.trim().parse::<i64>().map_err(|_| invalid(format!("The value '{s}' cannot be converted to an integer.")))?),
                    Value::Bool(b) => json!(i64::from(*b)),
                    other => return Err(invalid(format!("The value '{}' cannot be converted to an integer.", to_text(other)))),
                }
            }
            "float" => {
                need(1, 1)?;
                match &v[0] {
                    Value::Number(x) => json!(x.as_f64()),
                    Value::String(s) => json!(s.trim().parse::<f64>().map_err(|_| invalid(format!("The value '{s}' cannot be converted to a float.")))?),
                    other => return Err(invalid(format!("The value '{}' cannot be converted to a float.", to_text(other)))),
                }
            }
            "min" | "max" => {
                let items: Vec<Value> = match v {
                    [Value::Array(items)] => items.clone(),
                    _ => v.to_vec(),
                };
                if items.is_empty() {
                    return Err(invalid(format!("The template function '{name}' needs at least one value.")));
                }
                let mut best = int(&items[0], name)?;
                for item in &items[1..] {
                    let x = int(item, name)?;
                    best = if lower == "min" { best.min(x) } else { best.max(x) };
                }
                json!(best)
            }
            "range" => {
                need(2, 2)?;
                let (start, count) = (int(&v[0], name)?, int(&v[1], name)?);
                if !(0..=10000).contains(&count) {
                    return Err(invalid("The template function 'range' expects a count between 0 and 10000."));
                }
                Value::Array((start..start + count).map(|i| json!(i)).collect())
            }
            // ---- strings
            "string" => {
                need(1, 1)?;
                Value::String(to_text(&v[0]))
            }
            "concat" => {
                if v.iter().all(|x| x.is_array() || x.is_null()) && !v.is_empty() {
                    Value::Array(v.iter().filter_map(Value::as_array).flatten().cloned().collect())
                } else {
                    let mut out = String::new();
                    for item in v {
                        match item {
                            Value::Array(_) | Value::Object(_) => {
                                return Err(invalid("The template function 'concat' expects all strings or all arrays."))
                            }
                            other => out.push_str(&to_text(other)),
                        }
                    }
                    Value::String(out)
                }
            }
            "format" => {
                if n == 0 {
                    return Err(invalid("The template function 'format' needs a format string."));
                }
                Value::String(composite_format(string(&v[0], name)?, &v[1..])?)
            }
            "tolower" => {
                need(1, 1)?;
                Value::String(string(&v[0], name)?.to_lowercase())
            }
            "toupper" => {
                need(1, 1)?;
                Value::String(string(&v[0], name)?.to_uppercase())
            }
            "trim" => {
                need(1, 1)?;
                Value::String(string(&v[0], name)?.trim().to_string())
            }
            "startswith" | "endswith" => {
                need(2, 2)?;
                let (s, p) = (string(&v[0], name)?.to_lowercase(), string(&v[1], name)?.to_lowercase());
                Value::Bool(if lower == "startswith" { s.starts_with(&p) } else { s.ends_with(&p) })
            }
            "replace" => {
                need(3, 3)?;
                Value::String(string(&v[0], name)?.replace(string(&v[1], name)?, string(&v[2], name)?))
            }
            "split" => {
                need(2, 2)?;
                let text = string(&v[0], name)?;
                let delimiters: Vec<String> = match &v[1] {
                    Value::Array(items) => items.iter().map(to_text).collect(),
                    other => vec![string(other, name)?.to_string()],
                };
                let mut parts = vec![text.to_string()];
                for delimiter in delimiters.iter().filter(|d| !d.is_empty()) {
                    parts = parts.iter().flat_map(|p| p.split(delimiter.as_str()).map(String::from).collect::<Vec<_>>()).collect();
                }
                Value::Array(parts.into_iter().map(Value::String).collect())
            }
            "join" => {
                need(2, 2)?;
                let items = array(&v[0], name)?;
                Value::String(items.iter().map(to_text).collect::<Vec<_>>().join(string(&v[1], name)?))
            }
            "substring" => {
                need(2, 3)?;
                let chars: Vec<char> = string(&v[0], name)?.chars().collect();
                let start = int(&v[1], name)?;
                let length = match v.get(2) {
                    Some(l) => int(l, name)?,
                    None => chars.len() as i64 - start,
                };
                if start < 0 || length < 0 || (start + length) as usize > chars.len() {
                    return Err(invalid(format!(
                        "The index and length parameters must refer to a location within the string. The index parameter: '{start}', the length parameter: '{length}', the length of the string parameter: '{}'.",
                        chars.len()
                    )));
                }
                Value::String(chars[start as usize..(start + length) as usize].iter().collect())
            }
            "padleft" => {
                need(2, 3)?;
                let text = to_text(&v[0]);
                let total = int(&v[1], name)? as usize;
                let pad = match v.get(2) {
                    Some(p) => string(p, name)?.chars().next().unwrap_or(' '),
                    None => ' ',
                };
                let count = text.chars().count();
                let mut out: String = std::iter::repeat_n(pad, total.saturating_sub(count)).collect();
                out.push_str(&text);
                Value::String(out)
            }
            "indexof" | "lastindexof" => {
                need(2, 2)?;
                match &v[0] {
                    Value::Array(items) => {
                        let found = if lower == "indexof" {
                            items.iter().position(|x| values_equal(x, &v[1]))
                        } else {
                            items.iter().rposition(|x| values_equal(x, &v[1]))
                        };
                        json!(found.map(|i| i as i64).unwrap_or(-1))
                    }
                    other => {
                        let haystack: Vec<char> = string(other, name)?.to_lowercase().chars().collect();
                        let needle: Vec<char> = string(&v[1], name)?.to_lowercase().chars().collect();
                        if needle.is_empty() {
                            return Ok(json!(if lower == "indexof" { 0 } else { haystack.len() as i64 - 1 }.max(0)));
                        }
                        let mut positions = (0..=haystack.len().saturating_sub(needle.len()))
                            .filter(|i| haystack.len() >= needle.len() && haystack[*i..*i + needle.len()] == needle[..]);
                        let found = if lower == "indexof" { positions.next() } else { positions.next_back() };
                        json!(found.map(|i| i as i64).unwrap_or(-1))
                    }
                }
            }
            "base64" => {
                need(1, 1)?;
                Value::String(base64::engine::general_purpose::STANDARD.encode(string(&v[0], name)?))
            }
            "base64tostring" | "base64tojson" => {
                need(1, 1)?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(string(&v[0], name)?)
                    .map_err(|_| invalid("The value is not valid base64."))?;
                let text = String::from_utf8_lossy(&bytes).into_owned();
                if lower == "base64tojson" {
                    serde_json::from_str(&text).map_err(|_| invalid("The decoded value is not valid JSON."))?
                } else {
                    Value::String(text)
                }
            }
            "datauri" => {
                need(1, 1)?;
                let encoded = base64::engine::general_purpose::STANDARD.encode(to_text(&v[0]));
                Value::String(format!("data:text/plain;charset=utf8;base64,{encoded}"))
            }
            "datauritostring" => {
                need(1, 1)?;
                let text = string(&v[0], name)?;
                let (header, data) = text
                    .strip_prefix("data:")
                    .and_then(|t| t.split_once(','))
                    .ok_or_else(|| invalid(format!("The value '{text}' is not a data URI.")))?;
                if header.ends_with(";base64") {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .map_err(|_| invalid("The data URI is not valid base64."))?;
                    Value::String(String::from_utf8_lossy(&bytes).into_owned())
                } else {
                    Value::String(crate::http_util::percent_decode(data, false))
                }
            }
            "uri" => {
                need(2, 2)?;
                Value::String(resolve_uri(string(&v[0], name)?, string(&v[1], name)?))
            }
            "uricomponent" => {
                need(1, 1)?;
                Value::String(escape_data(string(&v[0], name)?))
            }
            "uricomponenttostring" => {
                need(1, 1)?;
                Value::String(crate::http_util::percent_decode(string(&v[0], name)?, false))
            }
            "guid" => {
                if n == 0 {
                    return Err(invalid("The template function 'guid' needs at least one parameter."));
                }
                let parts = v.iter().map(|x| string(x, name).map(String::from)).collect::<Result<Vec<_>, _>>()?;
                Value::String(template_guid(&parts))
            }
            "uniquestring" => {
                if n == 0 {
                    return Err(invalid("The template function 'uniqueString' needs at least one parameter."));
                }
                let parts = v.iter().map(|x| string(x, name).map(String::from)).collect::<Result<Vec<_>, _>>()?;
                Value::String(unique_string(&parts))
            }
            "newguid" => {
                need(0, 0)?;
                Value::String(uuid::Uuid::new_v4().to_string())
            }
            "utcnow" => {
                need(0, 1)?;
                let format = match v.first() {
                    Some(f) => string(f, name)?.to_string(),
                    None => "yyyyMMddTHHmmssZ".to_string(),
                };
                Value::String(format_time(&self.started, &format))
            }
            "datetimeadd" => {
                need(2, 3)?;
                let time = add_duration(parse_time(string(&v[0], name)?)?, string(&v[1], name)?)?;
                let format = match v.get(2) {
                    Some(f) => string(f, name)?.to_string(),
                    None => "yyyy-MM-ddTHH:mm:ssZ".to_string(),
                };
                Value::String(format_time(&time, &format))
            }
            "datetimetoepoch" => {
                need(1, 1)?;
                json!(parse_time(string(&v[0], name)?)?.timestamp())
            }
            "datetimefromepoch" => {
                need(1, 1)?;
                let time = chrono::DateTime::from_timestamp(int(&v[0], name)?, 0)
                    .ok_or_else(|| invalid("The epoch value is out of range."))?;
                Value::String(format_time(&time, "yyyy-MM-ddTHH:mm:ssZ"))
            }
            // ---- arrays and objects
            "array" => {
                need(1, 1)?;
                match &v[0] {
                    Value::Array(_) => v[0].clone(),
                    other => json!([other]),
                }
            }
            "createarray" => Value::Array(v.to_vec()),
            "createobject" => {
                if !n.is_multiple_of(2) {
                    return Err(invalid("The template function 'createObject' expects an even number of parameters."));
                }
                let mut out = Map::new();
                for pair in v.chunks(2) {
                    out.insert(string(&pair[0], name)?.to_string(), pair[1].clone());
                }
                Value::Object(out)
            }
            "json" => {
                need(1, 1)?;
                serde_json::from_str(string(&v[0], name)?)
                    .map_err(|_| invalid(format!("The template function 'json' was given invalid JSON: '{}'.", to_text(&v[0]))))?
            }
            "empty" => {
                need(1, 1)?;
                Value::Bool(match &v[0] {
                    Value::Null => true,
                    Value::String(s) => s.is_empty(),
                    Value::Array(a) => a.is_empty(),
                    Value::Object(o) => o.is_empty(),
                    _ => false,
                })
            }
            "length" => {
                need(1, 1)?;
                json!(match &v[0] {
                    Value::String(s) => s.chars().count(),
                    Value::Array(a) => a.len(),
                    Value::Object(o) => o.len(),
                    other => return Err(invalid(format!("The template function 'length' cannot take a value of type '{}'.", type_name(other)))),
                })
            }
            "contains" => {
                need(2, 2)?;
                Value::Bool(match &v[0] {
                    Value::String(s) => s.contains(&to_text(&v[1])),
                    Value::Array(a) => a.iter().any(|x| values_equal(x, &v[1])),
                    Value::Object(o) => {
                        let key = to_text(&v[1]);
                        o.keys().any(|k| k.eq_ignore_ascii_case(&key))
                    }
                    other => return Err(invalid(format!("The template function 'contains' cannot search a value of type '{}'.", type_name(other)))),
                })
            }
            "first" | "last" => {
                need(1, 1)?;
                match &v[0] {
                    Value::Array(a) => {
                        let item = if lower == "first" { a.first() } else { a.last() };
                        item.cloned().unwrap_or(Value::Null)
                    }
                    Value::String(s) => {
                        let c = if lower == "first" { s.chars().next() } else { s.chars().last() };
                        Value::String(c.map(String::from).unwrap_or_default())
                    }
                    other => return Err(invalid(format!("The template function '{name}' cannot take a value of type '{}'.", type_name(other)))),
                }
            }
            "skip" | "take" => {
                need(2, 2)?;
                let count = int(&v[1], name)?.max(0) as usize;
                match &v[0] {
                    Value::Array(a) => {
                        let count = count.min(a.len());
                        Value::Array(if lower == "skip" { a[count..].to_vec() } else { a[..count].to_vec() })
                    }
                    Value::String(s) => Value::String(if lower == "skip" {
                        s.chars().skip(count).collect()
                    } else {
                        s.chars().take(count).collect()
                    }),
                    other => return Err(invalid(format!("The template function '{name}' cannot take a value of type '{}'.", type_name(other)))),
                }
            }
            "union" => {
                if n == 0 {
                    return Err(invalid("The template function 'union' needs at least one parameter."));
                }
                let mut out = v[0].clone();
                for item in &v[1..] {
                    if out.is_object() != item.is_object() || out.is_array() != item.is_array() {
                        return Err(invalid("The template function 'union' expects all objects or all arrays."));
                    }
                    out = union_values(&out, item);
                }
                out
            }
            "intersection" => {
                if n == 0 {
                    return Err(invalid("The template function 'intersection' needs at least one parameter."));
                }
                match &v[0] {
                    Value::Array(first) => {
                        let mut out: Vec<Value> = Vec::new();
                        for item in first {
                            let everywhere = v[1..].iter().all(|o| o.as_array().is_some_and(|a| a.contains(item)));
                            if everywhere && !out.contains(item) {
                                out.push(item.clone());
                            }
                        }
                        Value::Array(out)
                    }
                    Value::Object(first) => {
                        let mut out = Map::new();
                        for (key, value) in first {
                            if v[1..].iter().all(|o| o.get(key) == Some(value)) {
                                out.insert(key.clone(), value.clone());
                            }
                        }
                        Value::Object(out)
                    }
                    _ => return Err(invalid("The template function 'intersection' expects arrays or objects.")),
                }
            }
            "flatten" => {
                need(1, 1)?;
                let mut out = Vec::new();
                for item in array(&v[0], name)? {
                    out.extend(array(item, name)?.iter().cloned());
                }
                Value::Array(out)
            }
            "items" => {
                need(1, 1)?;
                let mut entries: Vec<(&String, &Value)> = object(&v[0], name)?.iter().collect();
                entries.sort_by(|a, b| a.0.cmp(b.0));
                Value::Array(entries.into_iter().map(|(k, v)| json!({"key": k, "value": v})).collect())
            }
            "objectkeys" => {
                need(1, 1)?;
                Value::Array(object(&v[0], name)?.keys().map(|k| Value::String(k.clone())).collect())
            }
            "shallowmerge" => {
                need(1, 1)?;
                let mut out = Map::new();
                for item in array(&v[0], name)? {
                    for (key, value) in object(item, name)? {
                        out.insert(key.clone(), value.clone());
                    }
                }
                Value::Object(out)
            }
            "tryget" => {
                if n < 2 {
                    return Err(invalid("The template function 'tryGet' needs at least two parameters."));
                }
                let mut current = v[0].clone();
                for key in &v[1..] {
                    current = match (&current, key) {
                        (Value::Object(o), Value::String(k)) => member(o, k).cloned().unwrap_or(Value::Null),
                        (Value::Array(a), Value::Number(i)) => {
                            i.as_u64().and_then(|i| a.get(i as usize)).cloned().unwrap_or(Value::Null)
                        }
                        _ => Value::Null,
                    };
                }
                current
            }
            // ---- deployment values
            "parameters" => {
                need(1, 1)?;
                let wanted = string(&v[0], name)?;
                if let Some(args) = &locals.function_args {
                    return member(args, wanted).cloned().ok_or_else(|| {
                        invalid(format!("The function parameter '{wanted}' is not defined."))
                    });
                }
                self.parameter(wanted)?
            }
            "variables" => {
                need(1, 1)?;
                self.variable(string(&v[0], name)?)?
            }
            "copyindex" => {
                need(0, 2)?;
                let (loop_name, offset) = match v {
                    [] => (None, 0),
                    [Value::String(s)] => (Some(s.as_str()), 0),
                    [offset] => (None, int(offset, name)?),
                    [Value::String(s), offset] => (Some(s.as_str()), int(offset, name)?),
                    _ => return Err(invalid("The template function 'copyIndex' expects a loop name and an offset.")),
                };
                let found = match loop_name {
                    None => locals.copies.last(),
                    Some(wanted) => locals.copies.iter().rev().find(|(n, _)| n.eq_ignore_ascii_case(wanted)),
                };
                let (_, index) = found.ok_or_else(|| {
                    invalid("The template function 'copyIndex' is not expected at this location. It can only be used within a copy loop.")
                })?;
                json!(index + offset)
            }
            "deployment" => {
                need(0, 0)?;
                self.deployment_value()
            }
            "environment" => {
                need(0, 0)?;
                self.dispatch.environment()
            }
            "subscription" => {
                need(0, 0)?;
                self.subscription_value()
            }
            "tenant" => {
                need(0, 0)?;
                let tenant = &self.dispatch.config().tenant_id;
                json!({"id": format!("/tenants/{tenant}"), "tenantId": tenant, "countryCode": "US", "displayName": "Azure Emulator"})
            }
            "resourcegroup" => {
                need(0, 0)?;
                self.resource_group_value()?
            }
            "managementgroup" | "managementgroupresourceid" => {
                return Err(ArmError::not_implemented(format_args!("the template function '{name}'")))
            }
            "providers" => return Err(ArmError::not_implemented("the template function 'providers'")),
            "pickzones" => {
                // The emulator's regions have no availability zones.
                need(3, 5)?;
                json!([])
            }
            "resourceid" => self.resource_id(v, name)?,
            "subscriptionresourceid" => {
                let (subscription, rest) = match v.get(1).and_then(Value::as_str) {
                    Some(s) if s.contains('/') && v.first().and_then(Value::as_str).is_some_and(|f| !f.contains('/')) => {
                        (string(&v[0], name)?.to_string(), &v[1..])
                    }
                    _ => (self.subscription_id.clone(), v),
                };
                Value::String(compose_id(&format!("/subscriptions/{subscription}"), rest, name)?)
            }
            "tenantresourceid" => Value::String(compose_id("", v, name)?),
            "extensionresourceid" => {
                if n < 3 {
                    return Err(invalid("The template function 'extensionResourceId' needs a scope, a type and a name."));
                }
                Value::String(compose_id(string(&v[0], name)?.trim_end_matches('/'), &v[1..], name)?)
            }
            "reference" => {
                need(1, 3)?;
                let target = string(&v[0], name)?;
                let api_version = v.get(1).and_then(Value::as_str);
                let full = v.get(2).and_then(Value::as_str).is_some_and(|s| s.eq_ignore_ascii_case("full"));
                self.reference(target, api_version, full)?
            }
            "resourceinfo" => {
                need(1, 1)?;
                self.resource_info(string(&v[0], name)?)?
            }
            _ => return Err(invalid(format!("The template function '{name}' is not valid. Please see https://aka.ms/arm-functions for usage details."))),
        })
    }

    /// `resourceId([subscriptionId], [resourceGroupName], type, names...)`.
    fn resource_id(&self, v: &[Value], name: &str) -> Result<Value, ArmError> {
        let type_at = v
            .iter()
            .position(|x| x.as_str().is_some_and(|s| s.contains('/')))
            .ok_or_else(|| invalid("The template function 'resourceId' needs a resource type such as 'Microsoft.Storage/storageAccounts'."))?;
        let prefix: Vec<&str> = v[..type_at].iter().map(|x| string(x, name)).collect::<Result<_, _>>()?;
        let (subscription, group) = match prefix.as_slice() {
            [] => (self.subscription_id.clone(), self.resource_group.clone()),
            [group] => (self.subscription_id.clone(), Some(group.to_string())),
            [subscription, group] => (subscription.to_string(), Some(group.to_string())),
            _ => return Err(invalid("The template function 'resourceId' has too many leading parameters.")),
        };
        let base = match group {
            Some(group) => format!("/subscriptions/{subscription}/resourceGroups/{group}"),
            None => format!("/subscriptions/{subscription}"),
        };
        compose_id(&base, &v[type_at..], name).map(Value::String)
    }
}

/// `{base}/providers/{namespace}/{type}/{name}[/{type}/{name}...]` from a full
/// type and its names.
pub fn compose_id(base: &str, v: &[Value], function: &str) -> Result<String, ArmError> {
    let resource_type = v
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("The template function '{function}' needs a resource type.")))?;
    let names: Vec<String> = v[1..].iter().map(to_text).collect();
    // A name may itself hold several segments: `account/default`.
    let names: Vec<&str> = names.iter().flat_map(|n| n.split('/')).collect();
    id_from_parts(base, resource_type, &names).ok_or_else(|| {
        invalid(format!(
            "Unable to evaluate template language function '{function}': the type '{resource_type}' requires {} resource name argument(s).",
            resource_type.split('/').count() - 1
        ))
    })
}

/// Joins a full type (`Ns/a/b`) and its names into an id under `base`.
pub fn id_from_parts(base: &str, resource_type: &str, names: &[&str]) -> Option<String> {
    let segments: Vec<&str> = resource_type.split('/').collect();
    if segments.len() < 2 || segments.len() - 1 != names.len() {
        return None;
    }
    // Resource groups have their own path under a subscription.
    if resource_type.eq_ignore_ascii_case("Microsoft.Resources/resourceGroups")
        && base.starts_with("/subscriptions/")
        && base.matches('/').count() == 2
    {
        return Some(format!("{base}/resourceGroups/{}", names[0]));
    }
    let mut id = format!("{base}/providers/{}", segments[0]);
    for (kind, name) in segments[1..].iter().zip(names) {
        id.push_str(&format!("/{kind}/{name}"));
    }
    Some(id)
}

/// The `copy` loops of an object's properties: `[{name, count, input}]`.
fn property_copies(value: &Value) -> Option<Vec<(&str, &Value, &Value)>> {
    let loops = value.as_array()?;
    loops
        .iter()
        .map(|entry| {
            let entry = entry.as_object()?;
            Some((entry.get("name")?.as_str()?, entry.get("count")?, entry.get("input")?))
        })
        .collect()
}

/// `equals`: deep equality, with integers equal whatever their JSON form.
pub fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| values_equal(p, q)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| values_equal(v, w)))
        }
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_string_matches_resource_manager() {
        // Values computed by Microsoft's own expression evaluator (the Bicep CLI's
        // `build-params`).
        assert_eq!(unique_string(&["hello".into()]), "htpi75jfihg4e");
        assert_eq!(unique_string(&["a".into(), "b-c".into(), "resourcegroupid1234".into()]), "vbuwdlp3tp5eq");
        assert_eq!(
            unique_string(&["/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rg".into()]),
            "dojm7b5lc3trm"
        );
    }

    #[test]
    fn guid_matches_resource_manager() {
        assert_eq!(template_guid(&["hello".into()]), "520f8434-fe3a-5d99-888d-450a827486a1");
        assert_eq!(template_guid(&["a".into(), "b".into()]), "2d796349-8c7e-55ec-9624-54ece82ed031");
    }

    #[test]
    fn composite_formatting() {
        let args = [json!("x"), json!(1234567), json!(true)];
        assert_eq!(composite_format("{0}-{1:N0}-{2} {{literal}}", &args).unwrap(), "x-1,234,567-True {literal}");
        assert_eq!(composite_format("{1:D9}", &args).unwrap(), "001234567");
        assert!(composite_format("{3}", &args).is_err());
    }

    #[test]
    fn uris() {
        assert_eq!(resolve_uri("https://a.b/c/d.json", "e.json"), "https://a.b/c/e.json");
        assert_eq!(resolve_uri("https://a.b/c/", "/x"), "https://a.b/x");
        assert_eq!(resolve_uri("https://a.b", "x"), "https://a.b/x");
        assert_eq!(escape_data("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn times() {
        let t = parse_time("2020-04-07T14:53:14Z").unwrap();
        assert_eq!(format_time(&t, "yyyyMMddTHHmmssZ"), "20200407T145314Z");
        assert_eq!(format_time(&add_duration(t, "P1Y").unwrap(), "u"), "2021-04-07 14:53:14Z");
        assert_eq!(format_time(&add_duration(t, "-PT4H").unwrap(), "u"), "2020-04-07 10:53:14Z");
        assert_eq!(format_time(&add_duration(t, "-P1M").unwrap(), "u"), "2020-03-07 14:53:14Z");
    }
}
