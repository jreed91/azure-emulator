//! Validation of requests against the compact schemas `emulator-scaffold` emits,
//! and [`SchemaResource`]: a resource type served from its schema alone.
//!
//! A schema here is the subset of OpenAPI the scaffolder keeps: `type`, closed
//! `enum`s, string and number bounds, `pattern`, `required`, `readOnly`,
//! `default`, `createOnly` (members fixed at creation), `items`, `properties`,
//! `map` (free-form member names) and `open` (a polymorphic base whose subtypes
//! add members the schema cannot list).

use fancy_regex::Regex;
use serde_json::{json, Map, Value};

use crate::arm::{ArmContext, PutRequest, Resource, ResourceTypeHandler};
use crate::http_util::iso_timestamp;
use crate::ArmError;

fn matches_pattern(pattern: &str, text: &str) -> bool {
    // A pattern the engine cannot compile is not enforced rather than guessed at.
    Regex::new(pattern)
        .ok()
        .and_then(|regex| regex.is_match(text).ok())
        .unwrap_or(true)
}

fn bound(schema: &Value, key: &str) -> Option<f64> {
    schema.get(key).and_then(Value::as_f64)
}

/// Checks a resource name against the rules of its path parameter.
pub fn check_name(rules: &Value, name: &str) -> Result<(), String> {
    let length = name.chars().count() as f64;
    if bound(rules, "minLength").is_some_and(|min| length < min) || bound(rules, "maxLength").is_some_and(|max| length > max) {
        return Err(format!("The name '{name}' is not of a valid length."));
    }
    if let Some(pattern) = rules.get("pattern").and_then(Value::as_str) {
        if !matches_pattern(pattern, name) {
            return Err(format!("The name '{name}' does not match the pattern '{pattern}'."));
        }
    }
    Ok(())
}

/// How a request is checked.
#[derive(Clone, Copy)]
pub struct Mode {
    /// Fill absent members that have a default (creation only).
    pub apply_defaults: bool,
    /// Refuse a request that leaves out a required member (not for `PATCH`).
    pub enforce_required: bool,
}

fn type_error(path: &str, expected: &str) -> String {
    format!("The value of '{path}' is invalid: expected {expected}.")
}

/// Checks `value` against `schema` and returns what the service keeps of it:
/// read-only members are dropped (the caller cannot set them), members the
/// schema does not know are dropped unless the object is open, and defaults are
/// filled in when `mode` asks for it.
pub fn clean(value: &Value, schema: &Value, path: &str, mode: Mode) -> Result<Value, String> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => {
            let text = value.as_str().ok_or_else(|| type_error(path, "a string"))?;
            let length = text.chars().count() as f64;
            if bound(schema, "minLength").is_some_and(|min| length < min)
                || bound(schema, "maxLength").is_some_and(|max| length > max)
            {
                return Err(format!("The value of '{path}' is not of a valid length."));
            }
            if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
                if !matches_pattern(pattern, text) {
                    return Err(format!("The value of '{path}' does not match the pattern '{pattern}'."));
                }
            }
        }
        Some("integer") => {
            if value.as_i64().is_none() && value.as_u64().is_none() {
                return Err(type_error(path, "an integer"));
            }
        }
        Some("number") => {
            if !value.is_number() {
                return Err(type_error(path, "a number"));
            }
        }
        Some("boolean") => {
            if !value.is_boolean() {
                return Err(type_error(path, "true or false"));
            }
        }
        Some("array") => {
            let items = value.as_array().ok_or_else(|| type_error(path, "an array"))?;
            let count = items.len() as f64;
            if bound(schema, "minItems").is_some_and(|min| count < min) || bound(schema, "maxItems").is_some_and(|max| count > max) {
                return Err(format!("The value of '{path}' does not have a valid number of items."));
            }
            let item_schema = schema.get("items").cloned().unwrap_or_else(|| json!({}));
            let cleaned = items
                .iter()
                .enumerate()
                .map(|(index, item)| clean(item, &item_schema, &format!("{path}[{index}]"), mode))
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(Value::Array(cleaned));
        }
        Some("object") => return clean_object(value, schema, path, mode),
        _ => {}
    }
    if let Some(number) = value.as_f64() {
        if bound(schema, "minimum").is_some_and(|min| number < min) || bound(schema, "maximum").is_some_and(|max| number > max) {
            return Err(format!("The value of '{path}' is out of range."));
        }
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !allowed.contains(value) {
            let list: Vec<String> = allowed.iter().map(|v| v.to_string()).collect();
            return Err(format!("The value of '{path}' must be one of {}.", list.join(", ")));
        }
    }
    Ok(value.clone())
}

fn clean_object(value: &Value, schema: &Value, path: &str, mode: Mode) -> Result<Value, String> {
    let given = value.as_object().ok_or_else(|| type_error(path, "an object"))?;
    let known = schema.get("properties").and_then(Value::as_object);
    let open = schema.get("open").and_then(Value::as_bool).unwrap_or(false);
    let map = schema.get("map").and_then(Value::as_bool).unwrap_or(false);
    let Some(known) = known else {
        // Free-form: nothing to check member by member.
        return Ok(value.clone());
    };
    let mut out = Map::new();
    for (name, member) in given {
        let member_path = format!("{path}.{name}");
        match known.get(name) {
            Some(member_schema) if member_schema.get("readOnly") == Some(&Value::Bool(true)) => {}
            Some(member_schema) => {
                let cleaned = clean(member, member_schema, &member_path, mode)?;
                if !cleaned.is_null() {
                    out.insert(name.clone(), cleaned);
                }
            }
            None if open || map => {
                out.insert(name.clone(), member.clone());
            }
            None => {}
        }
    }
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    for (name, member_schema) in known {
        if out.contains_key(name) || member_schema.get("readOnly") == Some(&Value::Bool(true)) {
            continue;
        }
        if let Some(default) = member_schema.get("default").filter(|_| mode.apply_defaults) {
            out.insert(name.clone(), default.clone());
        } else if mode.enforce_required && required.contains(&name.as_str()) {
            return Err(format!("The required property '{path}.{name}' is missing."));
        }
    }
    Ok(Value::Object(out))
}

/// A resource type served entirely from its schema: the specification's rules
/// are enforced, and what a caller may set is stored and returned.
///
/// The service-owned values it fills in are only ones the specification makes
/// unambiguous: `provisioningState` (always `Succeeded`, since work completes
/// before the first response), a read-only `uuid` minted at creation, and
/// read-only creation and modification times. Every other read-only member is
/// left out rather than invented. Actions are not served.
pub struct SchemaResource {
    resource_type: &'static str,
    name_rules: Value,
    properties: Value,
    properties_required: bool,
}

impl SchemaResource {
    pub fn new(resource_type: &'static str, schema: &Value) -> Self {
        let body = schema.get("body").cloned().unwrap_or_else(|| json!({}));
        let properties_required = body
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|r| r.iter().any(|m| m == "properties"));
        Self {
            resource_type,
            name_rules: schema.get("name").cloned().unwrap_or_else(|| json!({})),
            properties: body.pointer("/properties/properties").cloned().unwrap_or_else(|| json!({})),
            properties_required,
        }
    }

    fn members(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.properties
            .get("properties")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
    }

    /// Fills in the service-owned values the specification makes unambiguous.
    fn stamp(&self, properties: &mut Value, existing: Option<&Value>) {
        let now = iso_timestamp();
        for (name, member) in self.members() {
            if member.get("readOnly") != Some(&Value::Bool(true)) {
                continue;
            }
            let lower = name.to_ascii_lowercase();
            let format = member.get("format").and_then(Value::as_str);
            let kept = existing.and_then(|e| e.get(name)).cloned();
            let value = if name == "provisioningState" {
                Some(json!("Succeeded"))
            } else if format == Some("uuid") {
                kept.or_else(|| Some(json!(uuid::Uuid::new_v4().to_string())))
            } else if lower.contains("modif") || lower.contains("updat") {
                Some(json!(now))
            } else if lower.contains("creat") {
                kept.or_else(|| Some(json!(now)))
            } else {
                kept
            };
            if let Some(value) = value {
                properties[name] = value;
            }
        }
    }

    /// Refuses a change to a member fixed at creation, and keeps its stored
    /// value when the request leaves it out.
    fn hold_create_only(&self, properties: &mut Value, existing: &Value) -> Result<(), ArmError> {
        for (name, member) in self.members() {
            if member.get("createOnly") != Some(&Value::Bool(true)) {
                continue;
            }
            match (properties.get(name), existing.get(name)) {
                (Some(new), Some(old)) if new != old => {
                    return Err(invalid(format!(
                        "The property 'properties.{name}' cannot be changed after the resource is created."
                    )))
                }
                (None, Some(old)) => properties[name] = old.clone(),
                _ => {}
            }
        }
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> ArmError {
    ArmError::bad_request("InvalidRequestContent", message)
}

impl ResourceTypeHandler for SchemaResource {
    fn resource_type(&self) -> &'static str {
        self.resource_type
    }

    fn put(&self, _ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        check_name(&self.name_rules, request.name).map_err(|message| ArmError::bad_request("InvalidResourceName", message))?;
        let given = request.body.get("properties").filter(|p| !p.is_null());
        if given.is_none() && self.properties_required {
            return Err(invalid("The required property 'properties' is missing."));
        }
        let mode = Mode {
            apply_defaults: request.existing.is_none(),
            enforce_required: true,
        };
        let empty = json!({});
        let mut properties = clean(given.unwrap_or(&empty), &self.properties, "properties", mode).map_err(invalid)?;
        let existing = request.existing.map(|r| &r.properties);
        if let Some(existing) = existing {
            self.hold_create_only(&mut properties, existing)?;
        }
        self.stamp(&mut properties, existing);
        Ok(properties)
    }

    fn patch(&self, _ctx: &ArmContext<'_>, existing: &Resource, body: &Value) -> Result<Value, ArmError> {
        let mode = Mode {
            apply_defaults: false,
            enforce_required: false,
        };
        let empty = json!({});
        let given = body.get("properties").filter(|p| !p.is_null()).unwrap_or(&empty);
        let changes = clean(given, &self.properties, "properties", mode).map_err(invalid)?;
        let mut properties = existing.properties.clone();
        if let (Some(target), Some(changes)) = (properties.as_object_mut(), changes.as_object()) {
            for (name, value) in changes {
                target.insert(name.clone(), value.clone());
            }
        }
        self.hold_create_only(&mut properties, &existing.properties)?;
        // A PATCH cannot change a member fixed at creation either.
        for (name, member) in self.members() {
            if member.get("createOnly") == Some(&Value::Bool(true)) && changes.get(name).is_some_and(|v| Some(v) != existing.properties.get(name)) {
                return Err(invalid(format!(
                    "The property 'properties.{name}' cannot be changed after the resource is created."
                )));
            }
        }
        self.stamp(&mut properties, Some(&existing.properties));
        Ok(properties)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREATE: Mode = Mode { apply_defaults: true, enforce_required: true };

    fn schema() -> Value {
        json!({
            "type": "object",
            "required": ["kind"],
            "properties": {
                "kind": {"type": "string", "enum": ["a", "b"]},
                "size": {"type": "integer", "minimum": 1, "maximum": 10, "default": 5},
                "label": {"type": "string", "maxLength": 3},
                "state": {"type": "string", "readOnly": true},
                "tags": {"type": "array", "items": {"type": "string", "pattern": "^[a-z]+$"}},
                "source": {"type": "object", "open": true, "properties": {"type": {"type": "string"}}},
            },
        })
    }

    #[test]
    fn cleans_and_fills_defaults() {
        let given = json!({"kind": "a", "state": "Hacked", "unknown": 1, "source": {"type": "x", "subtypeField": 2}});
        let cleaned = clean(&given, &schema(), "properties", CREATE).unwrap();
        assert_eq!(cleaned, json!({"kind": "a", "size": 5, "source": {"type": "x", "subtypeField": 2}}));
    }

    #[test]
    fn refuses_what_the_schema_forbids() {
        let refused = |given: Value| clean(&given, &schema(), "properties", CREATE).unwrap_err();
        assert!(refused(json!({})).contains("'properties.kind' is missing"));
        assert!(refused(json!({"kind": "c"})).contains("must be one of"));
        assert!(refused(json!({"kind": "a", "size": 11})).contains("out of range"));
        assert!(refused(json!({"kind": "a", "size": "big"})).contains("an integer"));
        assert!(refused(json!({"kind": "a", "label": "long"})).contains("length"));
        assert!(refused(json!({"kind": "a", "tags": ["ok", "NO"]})).contains("properties.tags[1]"));
    }

    #[test]
    fn names_follow_lookahead_patterns() {
        let rules = json!({"minLength": 3, "maxLength": 63, "pattern": "^[a-z0-9](?!.*--)[a-z0-9-]{1,61}[a-z0-9]$"});
        assert!(check_name(&rules, "good-name").is_ok());
        assert!(check_name(&rules, "bad--name").is_err());
        assert!(check_name(&rules, "ab").is_err());
    }
}
