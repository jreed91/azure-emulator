//! The `$management` node of an entity: request/response operations beside
//! sending and receiving.
//!
//! Implemented: `peek-message`, `renew-lock`, `update-disposition` (how the
//! SDKs complete, abandon, defer and dead-letter by lock token),
//! `schedule-message`, `cancel-scheduled-message` and
//! `receive-by-sequence-number` (deferred messages), and for sessions
//! `renew-session-lock`, `get-session-state` and `set-session-state`. Every
//! other operation (rules, listing sessions) answers status 501 rather than
//! pretending.

use std::time::SystemTime;

use emulator_core::Engine;
use fe2o3_amqp_types::messaging::message::__private::Serializable;
use fe2o3_amqp_types::messaging::{ApplicationProperties, Body, Message, Properties};
use fe2o3_amqp_types::primitives::{Array, OrderedMap, SimpleValue, Timestamp, Value};
use serde_bytes::ByteBuf;

use fe2o3_amqp_types::messaging::message::__private::Deserializable;

use super::store::{self, AmqpMessage, Entity, Settlement};

fn reply(request: &AmqpMessage, status: i32, description: &str, body: Value) -> AmqpMessage {
    let mut application = OrderedMap::new();
    application.insert("statusCode".to_string(), SimpleValue::Int(status));
    application.insert("statusDescription".to_string(), SimpleValue::String(description.to_string()));
    Message {
        header: None,
        delivery_annotations: None,
        message_annotations: None,
        properties: Some(Properties {
            correlation_id: request.properties.as_ref().and_then(|p| p.message_id.clone()),
            ..Default::default()
        }),
        application_properties: Some(ApplicationProperties(application)),
        body: Body::Value(fe2o3_amqp_types::messaging::AmqpValue(body)),
        footer: None,
    }
}

fn operation(request: &AmqpMessage) -> String {
    request
        .application_properties
        .as_ref()
        .and_then(|a| a.0.get("operation"))
        .and_then(|v| match v {
            SimpleValue::String(s) => Some(s.clone()),
            SimpleValue::Symbol(s) => Some(s.0.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The request body's map member `name`.
fn member<'a>(request: &'a AmqpMessage, name: &str) -> Option<&'a Value> {
    match &request.body {
        Body::Value(value) => match &value.0 {
            Value::Map(map) => map.get(&Value::String(name.to_string())).or_else(|| map.get(&Value::Symbol(name.into()))),
            _ => None,
        },
        _ => None,
    }
}

fn integer(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Long(n) => Some(*n),
        Value::Int(n) => Some(*n as i64),
        Value::Uint(n) => Some(*n as i64),
        Value::Ulong(n) => Some(*n as i64),
        Value::Short(n) => Some(*n as i64),
        _ => None,
    }
}

fn timestamp(time: SystemTime) -> Timestamp {
    let millis = time.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    Timestamp::from_milliseconds(millis)
}

/// Answers one management request addressed to `entity`.
pub(crate) fn handle(engine: &Engine, entity: &Entity, request: &AmqpMessage) -> AmqpMessage {
    let mut body = OrderedMap::new();
    match operation(request).as_str() {
        "com.microsoft:peek-message" => {
            let from = integer(member(request, "from-sequence-number")).unwrap_or(1);
            let count = integer(member(request, "message-count")).unwrap_or(1).clamp(1, 1000) as usize;
            let session = text(member(request, "session-id"));
            let messages = store::peek(engine, entity, from, count, session.as_deref());
            if messages.is_empty() {
                return reply(request, 204, "No messages", Value::Null);
            }
            let encoded: Vec<Value> = messages
                .into_iter()
                .filter_map(|message| serde_amqp::to_vec(&Serializable(message)).ok())
                .map(|bytes| {
                    let mut entry = OrderedMap::new();
                    entry.insert(Value::String("message".into()), Value::Binary(ByteBuf::from(bytes)));
                    Value::Map(entry)
                })
                .collect();
            body.insert(Value::String("messages".into()), Value::List(encoded));
            reply(request, 200, "OK", Value::Map(body))
        }
        "com.microsoft:renew-lock" => {
            let tokens = uuids(member(request, "lock-tokens"));
            let mut expirations = Vec::new();
            for token in tokens {
                match store::renew(engine, entity, token) {
                    Some(until) => expirations.push(Value::Timestamp(timestamp(until))),
                    None => return lock_lost(request),
                }
            }
            body.insert(Value::String("expirations".into()), Value::Array(Array::from(expirations)));
            reply(request, 200, "OK", Value::Map(body))
        }
        "com.microsoft:renew-session-lock" => {
            let Some(id) = text(member(request, "session-id")) else {
                return reply(request, 400, "A session-id is required.", Value::Null);
            };
            match store::renew_session(engine, entity, &id) {
                Some(until) => {
                    body.insert(Value::String("expiration".into()), Value::Timestamp(timestamp(until)));
                    reply(request, 200, "OK", Value::Map(body))
                }
                None => session_lock_lost(request),
            }
        }
        "com.microsoft:get-session-state" => {
            let Some(id) = text(member(request, "session-id")) else {
                return reply(request, 400, "A session-id is required.", Value::Null);
            };
            match store::session_state(engine, entity, &id) {
                Some(state) => {
                    let state = state.map_or(Value::Null, |bytes| Value::Binary(ByteBuf::from(bytes)));
                    body.insert(Value::String("session-state".into()), state);
                    reply(request, 200, "OK", Value::Map(body))
                }
                None => session_lock_lost(request),
            }
        }
        "com.microsoft:set-session-state" => {
            let Some(id) = text(member(request, "session-id")) else {
                return reply(request, 400, "A session-id is required.", Value::Null);
            };
            let state = match member(request, "session-state") {
                Some(Value::Binary(bytes)) => Some(bytes.to_vec()),
                Some(Value::String(text)) => Some(text.clone().into_bytes()),
                _ => None,
            };
            if store::set_session_state(engine, entity, &id, state) {
                reply(request, 200, "OK", Value::Null)
            } else {
                session_lock_lost(request)
            }
        }
        "com.microsoft:update-disposition" => {
            let tokens = uuids(member(request, "lock-tokens"));
            let text_member = |name: &str| match member(request, name) {
                Some(Value::String(s)) => Some(s.clone()),
                Some(Value::Symbol(s)) => Some(s.0.clone()),
                _ => None,
            };
            let properties = modifications(member(request, "properties-to-modify"));
            let status = text_member("disposition-status").unwrap_or_default();
            for token in tokens {
                let settlement = match status.as_str() {
                    "completed" => Settlement::Complete,
                    "abandoned" => Settlement::Abandon { failed: true, properties: properties.clone() },
                    "defered" | "deferred" => Settlement::Defer { properties: properties.clone() },
                    "suspended" => Settlement::DeadLetter {
                        reason: text_member("deadletter-reason").unwrap_or_default(),
                        description: text_member("deadletter-description").unwrap_or_default(),
                        properties: properties.clone(),
                    },
                    other => return reply(request, 400, &format!("'{other}' is not a disposition status."), Value::Null),
                };
                let settled = store::locked_sequence(engine, entity, token)
                    .is_some_and(|sequence| store::settle(engine, entity, sequence, token, settlement));
                if !settled {
                    return lock_lost(request);
                }
            }
            reply(request, 200, "OK", Value::Null)
        }
        "com.microsoft:schedule-message" => {
            let entries = match member(request, "messages") {
                Some(Value::List(list)) => list.clone(),
                Some(Value::Array(array)) => array.0.clone(),
                _ => Vec::new(),
            };
            let mut sequences = Vec::new();
            for entry in entries {
                let Value::Map(map) = entry else { continue };
                let Some(Value::Binary(bytes)) = map.get(&Value::String("message".into())) else { continue };
                let message = match serde_amqp::from_slice::<Deserializable<AmqpMessage>>(bytes) {
                    Ok(message) => message.0,
                    Err(e) => return reply(request, 400, &format!("A scheduled message could not be decoded: {e}"), Value::Null),
                };
                match store::send(engine, entity, message) {
                    Ok(sequence) => sequences.push(Value::Long(sequence)),
                    Err(refusal) => return reply(request, 400, &refusal.description, Value::Null),
                }
            }
            body.insert(Value::String("sequence-numbers".into()), Value::Array(Array::from(sequences)));
            reply(request, 200, "OK", Value::Map(body))
        }
        "com.microsoft:cancel-scheduled-message" => {
            store::cancel_scheduled(engine, entity, &longs(member(request, "sequence-numbers")));
            reply(request, 200, "OK", Value::Null)
        }
        "com.microsoft:receive-by-sequence-number" => {
            // Settle mode 0 (first) is receive-and-delete; 1 (second) peek-lock.
            let remove = integer(member(request, "receiver-settle-mode")) == Some(0);
            match store::receive_deferred(engine, entity, &longs(member(request, "sequence-numbers")), remove) {
                Ok(messages) => {
                    let encoded: Vec<Value> = messages
                        .into_iter()
                        .filter_map(|(token, message)| {
                            let bytes = serde_amqp::to_vec(&Serializable(message)).ok()?;
                            let mut entry = OrderedMap::new();
                            entry.insert(Value::String("message".into()), Value::Binary(ByteBuf::from(bytes)));
                            entry.insert(Value::String("lock-token".into()), Value::Uuid(token.into_bytes().into()));
                            Some(Value::Map(entry))
                        })
                        .collect();
                    body.insert(Value::String("messages".into()), Value::List(encoded));
                    reply(request, 200, "OK", Value::Map(body))
                }
                Err(refusal) => reply(request, 410, &refusal.description, Value::Null),
            }
        }
        other => reply(
            request,
            501,
            &format!("The management operation '{other}' is not implemented by the emulator yet."),
            Value::Null,
        ),
    }
}

fn as_uuid(value: &Value) -> Option<uuid::Uuid> {
    match value {
        Value::Uuid(uuid) => Some(uuid::Uuid::from_bytes(*uuid.as_inner())),
        _ => None,
    }
}

fn session_lock_lost(request: &AmqpMessage) -> AmqpMessage {
    reply(
        request,
        410,
        "The session lock has expired on the MessageSession. Accept a new MessageSession.",
        Value::Null,
    )
}

fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => Some(s.clone()),
        Value::Symbol(s) => Some(s.0.clone()),
        _ => None,
    }
}

fn lock_lost(request: &AmqpMessage) -> AmqpMessage {
    reply(
        request,
        410,
        "The lock supplied is invalid. Either the lock expired, or the message has already been removed from the queue.",
        Value::Null,
    )
}

fn list(value: Option<&Value>) -> &[Value] {
    match value {
        Some(Value::Array(array)) => &array.0,
        Some(Value::List(list)) => list,
        _ => &[],
    }
}

fn uuids(value: Option<&Value>) -> Vec<uuid::Uuid> {
    list(value).iter().filter_map(as_uuid).collect()
}

fn longs(value: Option<&Value>) -> Vec<i64> {
    list(value).iter().filter_map(|v| integer(Some(v))).collect()
}

/// `properties-to-modify`: application properties to set on settlement.
fn modifications(value: Option<&Value>) -> Vec<(String, SimpleValue)> {
    let Some(Value::Map(map)) = value else { return Vec::new() };
    map.iter()
        .filter_map(|(key, value)| {
            let key = match key {
                Value::String(s) => s.clone(),
                Value::Symbol(s) => s.0.clone(),
                _ => return None,
            };
            let value = match value {
                Value::String(s) => SimpleValue::String(s.clone()),
                Value::Bool(b) => SimpleValue::Bool(*b),
                Value::Int(n) => SimpleValue::Int(*n),
                Value::Long(n) => SimpleValue::Long(*n),
                Value::Double(n) => SimpleValue::Double(*n),
                _ => return None,
            };
            Some((key, value))
        })
        .collect()
}
