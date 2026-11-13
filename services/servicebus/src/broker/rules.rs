//! A subscription's rules as the `$management` node manages them
//! (`add-rule`, `remove-rule`, `enumerate-rules`). They are the same rules
//! Resource Manager lists: the rule resources under the subscription.

use emulator_core::arm::{ArmState, Resource};
use emulator_core::Engine;
use fe2o3_amqp_types::primitives::{OrderedMap, Value};
use serde_amqp::described::Described;
use serde_amqp::descriptor::Descriptor;
use serde_json::{json, Map, Value as Json};

use super::sql;
use super::store::Entity;

const RULE_DESCRIPTION: u64 = 0x0000_0137_0000_0004;
const EMPTY_ACTION: u64 = 0x0000_0137_0000_0005;
const SQL_ACTION: u64 = 0x0000_0137_0000_0006;
const SQL_FILTER: u64 = 0x0000_0013_7000_0006;
const TRUE_FILTER: u64 = 0x0000_0013_7000_0007;
const FALSE_FILTER: u64 = 0x0000_0013_7000_0008;
const CORRELATION_FILTER: u64 = 0x0000_0013_7000_0009;

const RULE_TYPE: &str = "Microsoft.ServiceBus/namespaces/topics/subscriptions/rules";

/// The correlation filter's members: their AMQP names and Resource Manager names.
const CORRELATION_MEMBERS: [(&str, &str); 8] = [
    ("correlation-id", "correlationId"),
    ("message-id", "messageId"),
    ("to", "to"),
    ("reply-to", "replyTo"),
    ("label", "label"),
    ("session-id", "sessionId"),
    ("reply-to-session-id", "replyToSessionId"),
    ("content-type", "contentType"),
];

fn key(map: &OrderedMap<Value, Value>, name: &str) -> Option<Value> {
    map.get(&Value::String(name.to_string())).or_else(|| map.get(&Value::Symbol(name.into()))).cloned()
}

fn text(value: Option<Value>) -> Option<String> {
    match value? {
        Value::String(s) => Some(s),
        Value::Symbol(s) => Some(s.0),
        _ => None,
    }
}

fn to_json(value: &Value) -> Json {
    match value {
        Value::Bool(b) => json!(b),
        Value::String(s) => json!(s),
        Value::Symbol(s) => json!(s.0),
        Value::Byte(n) => json!(n),
        Value::Short(n) => json!(n),
        Value::Int(n) => json!(n),
        Value::Long(n) => json!(n),
        Value::Ubyte(n) => json!(n),
        Value::Ushort(n) => json!(n),
        Value::Uint(n) => json!(n),
        Value::Ulong(n) => json!(n),
        Value::Float(n) => json!(n.into_inner()),
        Value::Double(n) => json!(n.into_inner()),
        _ => Json::Null,
    }
}

fn to_value(json: &Json) -> Value {
    match json {
        Json::Bool(b) => Value::Bool(*b),
        Json::String(s) => Value::String(s.clone()),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Value::Long(i),
            None => Value::Double(n.as_f64().unwrap_or_default().into()),
        },
        _ => Value::Null,
    }
}

/// A rule's Resource Manager properties from an AMQP `rule-description` map.
pub(crate) fn from_description(description: &Value) -> Result<Json, String> {
    let Value::Map(description) = description else {
        return Err("The rule-description must be a map.".into());
    };
    let mut properties = json!({ "action": {} });
    if let Some(Value::Map(filter)) = key(description, "sql-filter") {
        let expression = text(key(&filter, "expression")).ok_or("The sql-filter needs an expression.")?;
        sql::Filter::parse(&expression).map_err(|e| format!("The SQL filter '{expression}' is not valid: {e}"))?;
        properties["filterType"] = json!("SqlFilter");
        properties["sqlFilter"] = json!({ "sqlExpression": expression, "compatibilityLevel": 20 });
    } else if let Some(Value::Map(filter)) = key(description, "correlation-filter") {
        let mut correlation = Map::new();
        for (amqp, arm) in CORRELATION_MEMBERS {
            if let Some(value) = text(key(&filter, amqp)) {
                correlation.insert(arm.to_string(), json!(value));
            }
        }
        if let Some(Value::Map(user)) = key(&filter, "properties") {
            let user: Map<String, Json> = user
                .iter()
                .filter_map(|(k, v)| text(Some(k.clone())).map(|k| (k, to_json(v))))
                .collect();
            if !user.is_empty() {
                correlation.insert("properties".into(), Json::Object(user));
            }
        }
        if correlation.is_empty() {
            return Err("A correlation filter must set at least one property.".into());
        }
        properties["filterType"] = json!("CorrelationFilter");
        properties["correlationFilter"] = Json::Object(correlation);
    } else {
        return Err("The rule-description needs a sql-filter or a correlation-filter.".into());
    }
    if let Some(Value::Map(action)) = key(description, "sql-rule-action") {
        if let Some(expression) = text(key(&action, "expression")).filter(|e| !e.trim().is_empty()) {
            sql::Action::parse(&expression).map_err(|e| format!("The rule action '{expression}' is not valid: {e}"))?;
            properties["action"] = json!({ "sqlExpression": expression, "compatibilityLevel": 20 });
        }
    }
    Ok(properties)
}

fn described(code: u64, items: Vec<Value>) -> Value {
    Value::Described(Box::new(Described { descriptor: Descriptor::Code(code), value: Value::List(items) }))
}

/// An AMQP `rule-description` for a rule's Resource Manager properties.
pub(crate) fn to_description(name: &str, properties: &Json) -> Value {
    let filter = match properties.get("filterType").and_then(Json::as_str) {
        Some("CorrelationFilter") => {
            let filter = &properties["correlationFilter"];
            let mut items: Vec<Value> = CORRELATION_MEMBERS
                .iter()
                .map(|(_, arm)| filter.get(*arm).and_then(Json::as_str).map_or(Value::Null, |s| Value::String(s.into())))
                .collect();
            let mut user = OrderedMap::new();
            for (k, v) in filter.get("properties").and_then(Json::as_object).into_iter().flatten() {
                user.insert(Value::String(k.clone()), to_value(v));
            }
            items.push(Value::Map(user));
            described(CORRELATION_FILTER, items)
        }
        _ => {
            let expression = properties.pointer("/sqlFilter/sqlExpression").and_then(Json::as_str).unwrap_or("1=1");
            match expression.replace(' ', "").as_str() {
                "1=1" => described(TRUE_FILTER, vec![]),
                "1=0" => described(FALSE_FILTER, vec![]),
                _ => described(SQL_FILTER, vec![Value::String(expression.into()), Value::Int(20)]),
            }
        }
    };
    let action = match properties.pointer("/action/sqlExpression").and_then(Json::as_str).filter(|e| !e.trim().is_empty()) {
        Some(expression) => described(SQL_ACTION, vec![Value::String(expression.into()), Value::Int(20)]),
        None => described(EMPTY_ACTION, vec![]),
    };
    described(RULE_DESCRIPTION, vec![filter, action, Value::String(name.into())])
}

/// The subscription's rules, by name, in the order they were created.
pub(crate) fn list(engine: &Engine, subscription: &Entity) -> Vec<(String, Json)> {
    let state = engine.state::<ArmState>();
    let state = state.lock();
    state
        .children(&format!("{}/rules", subscription.resource.id))
        .map(|rule| (rule.name.clone(), rule.properties.clone()))
        .collect()
}

/// Adds a rule; `(status, reason)` if it cannot be added.
pub(crate) fn add(engine: &Engine, subscription: &Entity, name: &str, properties: Json) -> Result<(), (i32, String)> {
    let id = format!("{}/rules/{name}", subscription.resource.id);
    let state = engine.state::<ArmState>();
    let mut state = state.lock();
    if state.resource(&id).is_some() {
        return Err((409, format!("The messaging entity '{name}' already exists.")));
    }
    state.insert(Resource {
        id,
        name: name.to_string(),
        resource_type: RULE_TYPE.into(),
        location: None,
        tags: None,
        kind: None,
        sku: None,
        identity: None,
        properties,
    });
    Ok(())
}

/// Removes a rule; whether there was one.
pub(crate) fn remove(engine: &Engine, subscription: &Entity, name: &str) -> bool {
    let id = format!("{}/rules/{name}", subscription.resource.id);
    let state = engine.state::<ArmState>();
    let mut state = state.lock();
    state.remove(&id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: Vec<(&str, Value)>) -> Value {
        let mut map = OrderedMap::new();
        for (k, v) in entries {
            map.insert(Value::String(k.into()), v);
        }
        Value::Map(map)
    }

    #[test]
    fn descriptions_round_trip_through_properties() {
        let sql = map(vec![
            ("sql-filter", map(vec![("expression", Value::String("color = 'red'".into()))])),
            ("sql-rule-action", map(vec![("expression", Value::String("SET seen = TRUE".into()))])),
        ]);
        let properties = from_description(&sql).unwrap();
        assert_eq!(properties["sqlFilter"]["sqlExpression"], "color = 'red'");
        assert_eq!(properties["action"]["sqlExpression"], "SET seen = TRUE");
        let Value::Described(rule) = to_description("red", &properties) else { panic!() };
        assert_eq!(rule.descriptor, Descriptor::Code(RULE_DESCRIPTION));

        let correlation = map(vec![(
            "correlation-filter",
            map(vec![("label", Value::String("order".into())), ("properties", map(vec![("level", Value::Long(3))]))]),
        )]);
        let properties = from_description(&correlation).unwrap();
        assert_eq!(properties["correlationFilter"], json!({ "label": "order", "properties": { "level": 3 } }));

        assert!(from_description(&map(vec![("sql-filter", map(vec![("expression", Value::String("x =".into()))]))])).is_err());
        assert!(from_description(&map(vec![])).is_err());
    }
}
