//! Subscription rules: whether a message sent to a topic matches a rule, and
//! what the rule's action does to the subscription's copy.
//!
//! Correlation filters compare every property they set. SQL filters and
//! actions are evaluated by [`super::sql`].

use fe2o3_amqp_types::messaging::MessageId;
use fe2o3_amqp_types::primitives::SimpleValue;
use serde_json::Value as Json;

use super::sql::{Action, Filter};
use super::store::{refuse, AmqpMessage, Refusal};

/// Whether the rule with these `properties` matches `message`.
pub(crate) fn matches(rule: &Json, message: &AmqpMessage) -> Result<bool, Refusal> {
    match rule.get("filterType").and_then(Json::as_str).unwrap_or("SqlFilter") {
        "CorrelationFilter" => Ok(correlation(rule.get("correlationFilter").unwrap_or(&Json::Null), message)),
        _ => {
            let expression = rule.pointer("/sqlFilter/sqlExpression").and_then(Json::as_str).unwrap_or("1=1");
            let filter = Filter::parse(expression)
                .map_err(|e| refuse("amqp:internal-error", format!("The rule's SQL filter '{expression}' is not valid: {e}")))?;
            Ok(filter.matches(message))
        }
    }
}

/// The rule's action, if it has one.
pub(crate) fn action(rule: &Json) -> Result<Option<Action>, Refusal> {
    match rule.pointer("/action/sqlExpression").and_then(Json::as_str).filter(|a| !a.trim().is_empty()) {
        None => Ok(None),
        Some(text) => Action::parse(text)
            .map(Some)
            .map_err(|e| refuse("amqp:internal-error", format!("The rule's action '{text}' is not valid: {e}"))),
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
