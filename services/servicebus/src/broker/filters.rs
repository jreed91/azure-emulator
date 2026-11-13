//! Subscription rules: whether a message sent to a topic matches a rule.
//!
//! Correlation filters are evaluated completely: every property the filter sets
//! must equal the message's. SQL filters are a language; until the emulator has
//! an evaluator for the whole of it, only the always-true and always-false
//! expressions (`1=1`, `1=0`, the forms `$Default` and the SDKs write) are
//! evaluated, and any other expression refuses the send rather than guessing.
//! Rule actions are refused the same way.

use fe2o3_amqp_types::messaging::MessageId;
use fe2o3_amqp_types::primitives::SimpleValue;
use serde_json::Value as Json;

use super::store::{refuse, AmqpMessage, Refusal};

/// Whether the rule with these `properties` matches `message`.
pub(crate) fn matches(rule: &Json, message: &AmqpMessage) -> Result<bool, Refusal> {
    if let Some(action) = rule.pointer("/action/sqlExpression").and_then(Json::as_str).filter(|a| !a.trim().is_empty()) {
        return Err(refuse(
            "amqp:not-implemented",
            format!("Rule actions ('{action}') are not supported by the emulator yet."),
        ));
    }
    match rule.get("filterType").and_then(Json::as_str).unwrap_or("SqlFilter") {
        "CorrelationFilter" => Ok(correlation(rule.get("correlationFilter").unwrap_or(&Json::Null), message)),
        _ => sql(rule.pointer("/sqlFilter/sqlExpression").and_then(Json::as_str).unwrap_or("1=1")),
    }
}

fn sql(expression: &str) -> Result<bool, Refusal> {
    let compact: String = expression.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    match compact.as_str() {
        "1=1" | "true" => Ok(true),
        "1=0" | "false" => Ok(false),
        _ => Err(refuse(
            "amqp:not-implemented",
            format!("The SQL filter '{expression}' cannot be evaluated: the emulator evaluates only 1=1 and 1=0 so far. Use a correlation filter instead."),
        )),
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

/// An application property's value as the text a correlation filter compares.
fn simple_text(value: &SimpleValue) -> Option<String> {
    Some(match value {
        SimpleValue::String(text) => text.clone(),
        SimpleValue::Symbol(symbol) => symbol.0.clone(),
        SimpleValue::Bool(b) => b.to_string(),
        SimpleValue::Ubyte(n) => n.to_string(),
        SimpleValue::Ushort(n) => n.to_string(),
        SimpleValue::Uint(n) => n.to_string(),
        SimpleValue::Ulong(n) => n.to_string(),
        SimpleValue::Byte(n) => n.to_string(),
        SimpleValue::Short(n) => n.to_string(),
        SimpleValue::Int(n) => n.to_string(),
        SimpleValue::Long(n) => n.to_string(),
        _ => return None,
    })
}

fn json_text(value: &Json) -> Option<String> {
    match value {
        Json::String(text) => Some(text.clone()),
        Json::Bool(b) => Some(b.to_string()),
        Json::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Every property the filter sets must equal the message's.
fn correlation(filter: &Json, message: &AmqpMessage) -> bool {
    let properties = message.properties.as_ref();
    let system: [(&str, Option<String>); 8] = [
        ("correlationId", properties.and_then(|p| p.correlation_id.as_ref()).map(id_text)),
        ("messageId", properties.and_then(|p| p.message_id.as_ref()).map(id_text)),
        ("to", properties.and_then(|p| p.to.clone())),
        ("replyTo", properties.and_then(|p| p.reply_to.clone())),
        ("label", properties.and_then(|p| p.subject.clone())),
        ("sessionId", properties.and_then(|p| p.group_id.clone())),
        ("replyToSessionId", properties.and_then(|p| p.reply_to_group_id.clone())),
        ("contentType", properties.and_then(|p| p.content_type.as_ref()).map(|s| s.0.clone())),
    ];
    for (name, actual) in system {
        if let Some(expected) = filter.get(name).and_then(Json::as_str) {
            if actual.as_deref() != Some(expected) {
                return false;
            }
        }
    }
    let application = message.application_properties.as_ref();
    for (name, expected) in filter.get("properties").and_then(Json::as_object).into_iter().flatten() {
        let actual = application.and_then(|a| a.0.get(name)).and_then(simple_text);
        if actual.is_none() || actual != json_text(expected) {
            return false;
        }
    }
    true
}
