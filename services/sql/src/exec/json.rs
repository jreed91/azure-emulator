//! JSON functions: `JSON_VALUE`, `JSON_QUERY`, `ISJSON`, `JSON_MODIFY`, `OPENJSON` and `FOR JSON`.

use serde_json::{Map, Value as Json};

use super::query::{Layout, LayoutColumn, Rowset};
use super::SqlError;
use crate::ast::ForJson;
use crate::value::{SqlType, Value};

enum Step {
    Key(String),
    Index(usize),
}

struct Path {
    strict: bool,
    steps: Vec<Step>,
}

fn path_error(path: &str) -> SqlError {
    SqlError::new(13607, 16, format!("JSON path is not properly formatted. Unexpected character '{}' is found at position 0.", path.chars().next().unwrap_or(' ')))
}

fn parse_path(text: &str) -> Result<Path, SqlError> {
    let mut rest = text.trim_start();
    let mut strict = false;
    if let Some(r) = rest.strip_prefix("strict") {
        strict = true;
        rest = r.trim_start();
    } else if let Some(r) = rest.strip_prefix("lax") {
        rest = r.trim_start();
    }
    let Some(mut rest) = rest.strip_prefix('$') else { return Err(path_error(text)) };
    let mut steps = Vec::new();
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('.') {
            if let Some(r) = r.strip_prefix('"') {
                let end = r.find('"').ok_or_else(|| path_error(text))?;
                steps.push(Step::Key(r[..end].to_string()));
                rest = &r[end + 1..];
            } else {
                let end = r.find(['.', '[']).unwrap_or(r.len());
                if end == 0 {
                    return Err(path_error(text));
                }
                steps.push(Step::Key(r[..end].to_string()));
                rest = &r[end..];
            }
        } else if let Some(r) = rest.strip_prefix('[') {
            let end = r.find(']').ok_or_else(|| path_error(text))?;
            let index = r[..end].trim().parse::<usize>().map_err(|_| path_error(text))?;
            steps.push(Step::Index(index));
            rest = &r[end + 1..];
        } else {
            return Err(path_error(text));
        }
    }
    Ok(Path { strict, steps })
}

fn parse_json(text: &str) -> Result<Json, SqlError> {
    serde_json::from_str(text).map_err(|e| {
        SqlError::new(13609, 16, format!("JSON text is not properly formatted. Unexpected character is found at position {}.", e.column().saturating_sub(1)))
    })
}

fn walk<'j>(json: &'j Json, path: &Path) -> Option<&'j Json> {
    let mut current = json;
    for step in &path.steps {
        current = match (step, current) {
            (Step::Key(k), Json::Object(map)) => map.get(k)?,
            (Step::Index(i), Json::Array(items)) => items.get(*i)?,
            _ => return None,
        };
    }
    Some(current)
}

fn scalar_text(json: &Json) -> Option<String> {
    match json {
        Json::String(s) => Some(s.clone()),
        Json::Number(n) => Some(n.to_string()),
        Json::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

pub fn json_value(json: &Value, path: &Value) -> Result<Value, SqlError> {
    let (Some(text), Some(path_text)) = (json.as_string(), path.as_string()) else { return Ok(Value::Null) };
    let path = parse_path(&path_text)?;
    let doc = parse_json(&text)?;
    match walk(&doc, &path) {
        Some(Json::Null) => Ok(Value::Null),
        Some(found) => match scalar_text(found) {
            Some(s) => {
                if s.chars().count() > 4000 {
                    if path.strict {
                        return Err(SqlError::new(13625, 16, "String value in the specified JSON path would be truncated."));
                    }
                    return Ok(Value::Null);
                }
                Ok(Value::Str(s))
            }
            None if path.strict => Err(SqlError::new(13623, 16, "Scalar value cannot be found in the specified JSON path.")),
            None => Ok(Value::Null),
        },
        None if path.strict => Err(SqlError::new(13608, 16, "Property cannot be found on the specified JSON path.")),
        None => Ok(Value::Null),
    }
}

pub fn json_query(json: &Value, path: Option<Value>) -> Result<Value, SqlError> {
    let Some(text) = json.as_string() else { return Ok(Value::Null) };
    let path_text = path.and_then(|p| p.as_string()).unwrap_or_else(|| "$".into());
    let path = parse_path(&path_text)?;
    let doc = parse_json(&text)?;
    match walk(&doc, &path) {
        Some(found @ (Json::Object(_) | Json::Array(_))) => Ok(Value::Str(found.to_string())),
        Some(_) if path.strict => Err(SqlError::new(13624, 16, "Object or array cannot be found in the specified JSON path.")),
        None if path.strict => Err(SqlError::new(13608, 16, "Property cannot be found on the specified JSON path.")),
        _ => Ok(Value::Null),
    }
}

pub fn is_json(json: &Value) -> Value {
    match json.as_string() {
        None => Value::Null,
        Some(text) => Value::Int(matches!(serde_json::from_str::<Json>(&text), Ok(Json::Object(_) | Json::Array(_))) as i64),
    }
}

fn to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bit(b) => Json::Bool(*b),
        Value::Int(i) => Json::from(*i),
        Value::Float(f) => serde_json::Number::from_f64(*f).map_or(Json::Null, Json::Number),
        Value::Decimal(d) => serde_json::from_str(&d.to_string()).unwrap_or(Json::Null),
        Value::Str(s) => Json::String(s.clone()),
        Value::DateTime(dt) => {
            let mut s = dt.format("%Y-%m-%dT%H:%M:%S%.f").to_string();
            if s.ends_with('.') {
                s.pop();
            }
            Json::String(s)
        }
        Value::Date(d) => Json::String(d.format("%Y-%m-%d").to_string()),
        Value::DateTimeOffset(dt) => Json::String(dt.to_rfc3339()),
        Value::Time(t) => Json::String(t.format("%H:%M:%S%.f").to_string()),
        Value::Guid(g) => Json::String(g.to_string().to_uppercase()),
        Value::Bytes(b) => Json::String(base64_encode(b)),
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn json_modify(json: &Value, path: &Value, new_value: &Value, new_expr: Option<&crate::ast::Expr>) -> Result<Value, SqlError> {
    let (Some(text), Some(path_text)) = (json.as_string(), path.as_string()) else { return Ok(Value::Null) };
    let mut path_text = path_text.trim().to_string();
    let append = path_text.starts_with("append ");
    if append {
        path_text = path_text["append ".len()..].to_string();
    }
    let path = parse_path(&path_text)?;
    let mut doc = parse_json(&text)?;
    // A value from JSON_QUERY is inserted as JSON, anything else as a scalar.
    let raw = matches!(new_expr, Some(crate::ast::Expr::Function { name, .. }) if name == "JSON_QUERY");
    let replacement = match (raw, new_value) {
        (true, Value::Str(s)) => parse_json(s)?,
        _ => to_json(new_value),
    };
    let Some((last, parents)) = path.steps.split_last() else {
        return Ok(Value::Str(replacement.to_string()));
    };
    let mut target = &mut doc;
    for step in parents {
        target = match (step, target) {
            (Step::Key(k), Json::Object(map)) => map.entry(k.clone()).or_insert_with(|| Json::Object(Map::new())),
            (Step::Index(i), Json::Array(items)) => match items.get_mut(*i) {
                Some(item) => item,
                None => return Ok(Value::Str(text)),
            },
            _ => return Ok(Value::Str(text)),
        };
    }
    match (last, target) {
        (Step::Key(k), Json::Object(map)) => {
            if append {
                let entry = map.entry(k.clone()).or_insert_with(|| Json::Array(Vec::new()));
                if let Json::Array(items) = entry {
                    items.push(replacement);
                }
            } else if replacement.is_null() && !path.strict {
                map.remove(k);
            } else {
                map.insert(k.clone(), replacement);
            }
        }
        (Step::Index(i), Json::Array(items)) => {
            if let Some(item) = items.get_mut(*i) {
                if append {
                    if let Json::Array(inner) = item {
                        inner.push(replacement);
                    }
                } else {
                    *item = replacement;
                }
            }
        }
        _ => {}
    }
    Ok(Value::Str(doc.to_string()))
}

/// `OPENJSON(json [, path]) [WITH (...)]`.
pub fn openjson(args: &[Value], with: &[(String, SqlType, Option<String>, bool)]) -> Result<Rowset, SqlError> {
    let text = args.first().and_then(Value::as_string);
    let path = match args.get(1).and_then(Value::as_string) {
        Some(p) => parse_path(&p)?,
        None => Path { strict: false, steps: Vec::new() },
    };
    let column = |name: &str, ty: SqlType| LayoutColumn { qualifier: None, name: name.to_string(), ty, nullable: true, identity: false, hidden: false };
    let layout = if with.is_empty() {
        Layout { columns: vec![column("key", SqlType::NVarChar(Some(4000))), column("value", SqlType::NVarChar(None)), column("type", SqlType::TinyInt)] }
    } else {
        Layout { columns: with.iter().map(|(name, ty, _, as_json)| column(name, if *as_json { SqlType::NVarChar(None) } else { ty.clone() })).collect() }
    };
    let Some(text) = text else { return Ok(Rowset { layout, rows: Vec::new() }) };
    let doc = parse_json(&text)?;
    let Some(target) = walk(&doc, &path) else {
        if path.strict {
            return Err(SqlError::new(13608, 16, "Property cannot be found on the specified JSON path."));
        }
        return Ok(Rowset { layout, rows: Vec::new() });
    };
    let entries: Vec<(String, &Json)> = match target {
        Json::Object(map) => map.iter().map(|(k, v)| (k.clone(), v)).collect(),
        Json::Array(items) => items.iter().enumerate().map(|(i, v)| (i.to_string(), v)).collect(),
        _ => return Ok(Rowset { layout, rows: Vec::new() }),
    };
    let mut rows = Vec::new();
    for (key, value) in entries {
        if with.is_empty() {
            let (text, kind) = match value {
                Json::Null => (Value::Null, 0),
                Json::String(s) => (Value::Str(s.clone()), 1),
                Json::Number(n) => (Value::Str(n.to_string()), 2),
                Json::Bool(b) => (Value::Str(b.to_string()), 3),
                Json::Array(_) => (Value::Str(value.to_string()), 4),
                Json::Object(_) => (Value::Str(value.to_string()), 5),
            };
            rows.push(vec![Value::Str(key), text, Value::Int(kind)]);
        } else {
            let mut row = Vec::new();
            for (name, ty, column_path, as_json) in with {
                let p = match column_path {
                    Some(p) => parse_path(p)?,
                    None => Path { strict: false, steps: vec![Step::Key(name.clone())] },
                };
                let found = walk(value, &p);
                let v = match found {
                    None | Some(Json::Null) => Value::Null,
                    Some(j @ (Json::Object(_) | Json::Array(_))) => {
                        if *as_json {
                            Value::Str(j.to_string())
                        } else {
                            Value::Null
                        }
                    }
                    Some(j) if *as_json => {
                        let _ = j;
                        Value::Null
                    }
                    Some(Json::Bool(b)) => Value::Bit(*b).convert(ty)?,
                    Some(j) => Value::Str(scalar_text(j).unwrap_or_default()).convert(ty)?,
                };
                row.push(v);
            }
            rows.push(row);
        }
    }
    Ok(Rowset { layout, rows })
}

/// The text `FOR JSON` produces for a result.
pub fn for_json(rowset: &Rowset, options: &ForJson) -> String {
    let columns: Vec<&LayoutColumn> = rowset.layout.columns.iter().filter(|c| !c.hidden).collect();
    let mut objects = Vec::new();
    for row in &rowset.rows {
        let mut object = Map::new();
        let visible = rowset.layout.columns.iter().zip(row).filter(|(c, _)| !c.hidden);
        for ((column, value), _) in visible.zip(columns.iter()) {
            if value.is_null() && !options.include_nulls {
                continue;
            }
            let mut json = to_json(value);
            // A nested FOR JSON or JSON_QUERY result is JSON already.
            if column.ty == SqlType::NVarChar(None) {
                if let Value::Str(s) = value {
                    let t = s.trim_start();
                    if t.starts_with('[') || t.starts_with('{') {
                        if let Ok(parsed) = serde_json::from_str::<Json>(s) {
                            json = parsed;
                        }
                    }
                }
            }
            let name = if column.name.is_empty() { "" } else { column.name.as_str() };
            if options.path && name.contains('.') {
                let parts: Vec<&str> = name.split('.').collect();
                insert_nested(&mut object, &parts, json);
            } else {
                object.insert(name.to_string(), json);
            }
        }
        objects.push(Json::Object(object));
    }
    let body = if options.without_array_wrapper {
        objects.iter().map(Json::to_string).collect::<Vec<_>>().join(",")
    } else {
        Json::Array(objects).to_string()
    };
    match &options.root {
        Some(root) => format!("{{{}:{}}}", Json::String(root.clone()), if body.is_empty() { "[]".into() } else { body }),
        None => body,
    }
}

fn insert_nested(object: &mut Map<String, Json>, parts: &[&str], value: Json) {
    match parts {
        [] => {}
        [last] => {
            object.insert(last.to_string(), value);
        }
        [first, rest @ ..] => {
            let entry = object.entry(first.to_string()).or_insert_with(|| Json::Object(Map::new()));
            if let Json::Object(inner) = entry {
                insert_nested(inner, rest, value);
            }
        }
    }
}
