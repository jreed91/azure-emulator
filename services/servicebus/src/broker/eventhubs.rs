//! Event Hubs over the same AMQP broker as Service Bus.
//!
//! An event hub is a set of partitions, each an append-only log. Senders
//! write to the hub (the partition chosen by the event's partition key, or
//! round robin) or to one partition (`<hub>/Partitions/<n>`). Receivers read
//! one partition of one consumer group
//! (`<hub>/ConsumerGroups/<group>/Partitions/<n>`) from the position their
//! `apache.org:selector-filter:string` names, and keep reading as events
//! arrive; reading takes nothing away. A receiver with an owner level
//! (`com.microsoft:epoch`) takes the partition from receivers with a lower
//! one. The namespace's `$management` node answers `READ` for a hub's and a
//! partition's properties.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use emulator_core::arm::ArmState;
use emulator_core::Engine;
use fe2o3_amqp::link::delivery::DeliveryInfo;
use fe2o3_amqp::{Receiver, Sender};
use fe2o3_amqp_types::messaging::{AmqpValue, ApplicationProperties, Body, Message, MessageAnnotations, Properties};
use fe2o3_amqp_types::primitives::{Array, OrderedMap, SimpleValue, Symbol, Timestamp, Value};
use serde_json::Value as Json;
use tokio::sync::Notify;

use super::store::{self, AmqpMessage};
use super::{error, Context};

const NAMESPACE_TYPE: &str = "Microsoft.EventHub/namespaces";
/// The largest event a Standard namespace takes.
pub(crate) const MAX_EVENT_SIZE: u64 = 1024 * 1024;

/// One event in a partition's log.
struct Event {
    sequence: i64,
    offset: i64,
    enqueued: SystemTime,
    message: AmqpMessage,
}

#[derive(Default)]
struct Partition {
    events: VecDeque<Event>,
    next_sequence: i64,
    next_offset: i64,
    /// Per consumer group: the owner level of the receiver holding the
    /// partition, if any, and a generation that moves when it is taken.
    owners: HashMap<String, (Option<i64>, u64)>,
}

#[derive(Default)]
struct Hub {
    partitions: Vec<Partition>,
    next_partition: usize,
}

/// Every event hub's partitions, keyed by `namespace/hub` (lowercased).
#[derive(Default)]
pub struct EventHubsState {
    hubs: HashMap<String, Hub>,
    notifiers: HashMap<String, Arc<Notify>>,
}

impl EventHubsState {
    fn notifier(&mut self, key: &str) -> Arc<Notify> {
        self.notifiers.entry(key.to_string()).or_insert_with(|| Arc::new(Notify::new())).clone()
    }
}

/// An event hub as Resource Manager describes it.
pub(crate) struct HubInfo {
    key: String,
    name: String,
    partition_count: usize,
    retention: Duration,
    created: SystemTime,
    resource_id: String,
}

/// Whether `namespace` is an Event Hubs namespace.
pub(crate) fn is_event_hubs_namespace(engine: &Engine, namespace: &str) -> bool {
    let state = engine.state::<ArmState>();
    let state = state.lock();
    let found = state.resources().any(|r| r.resource_type.eq_ignore_ascii_case(NAMESPACE_TYPE) && r.name.eq_ignore_ascii_case(namespace));
    found
}

fn hub_info(engine: &Engine, namespace: &str, name: &str) -> Result<HubInfo, String> {
    let state = engine.state::<ArmState>();
    let state = state.lock();
    let ns = state
        .resources()
        .find(|r| r.resource_type.eq_ignore_ascii_case(NAMESPACE_TYPE) && r.name.eq_ignore_ascii_case(namespace))
        .ok_or_else(|| format!("The messaging entity '{namespace}' could not be found."))?;
    let id = format!("{}/eventhubs/{name}", ns.id);
    let hub = state.resource(&id).ok_or_else(|| format!("The messaging entity '{namespace}/{name}' could not be found."))?;
    let partition_count = hub.properties.get("partitionCount").and_then(Json::as_u64).unwrap_or(4).max(1) as usize;
    let days = hub.properties.get("messageRetentionInDays").and_then(Json::as_u64).unwrap_or(1);
    let hours = hub.properties.pointer("/retentionDescription/retentionTimeInHours").and_then(Json::as_u64);
    let retention = Duration::from_secs(hours.map_or(days * 24 * 3600, |h| h * 3600));
    let created = hub
        .properties
        .get("createdAt")
        .and_then(Json::as_str)
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map_or(SystemTime::now(), |t| UNIX_EPOCH + Duration::from_millis(t.timestamp_millis().max(0) as u64));
    Ok(HubInfo { key: format!("{namespace}/{}", hub.name).to_ascii_lowercase(), name: hub.name.clone(), partition_count, retention, created, resource_id: hub.id.clone() })
}

fn consumer_group_exists(engine: &Engine, hub: &HubInfo, group: &str) -> bool {
    let state = engine.state::<ArmState>();
    let state = state.lock();
    let exists = state.resource(&format!("{}/consumergroups/{group}", hub.resource_id)).is_some();
    exists
}

/// Drops the events of hubs under `prefix` (`namespace` or `namespace/hub`).
pub fn forget(engine: &Engine, prefix: &str) {
    let prefix = prefix.to_ascii_lowercase();
    let state = engine.state::<EventHubsState>();
    let mut state = state.lock();
    state.hubs.retain(|key, _| key != &prefix && !key.starts_with(&format!("{prefix}/")));
}

fn with_hub<T>(engine: &Engine, hub: &HubInfo, f: impl FnOnce(&mut Hub) -> T) -> T {
    let state = engine.state::<EventHubsState>();
    let mut state = state.lock();
    let entry = state.hubs.entry(hub.key.clone()).or_default();
    while entry.partitions.len() < hub.partition_count {
        entry.partitions.push(Partition::default());
    }
    // Retention: events older than the hub keeps are gone.
    let now = SystemTime::now();
    for partition in &mut entry.partitions {
        while partition.events.front().is_some_and(|e| now.duration_since(e.enqueued).unwrap_or_default() > hub.retention) {
            partition.events.pop_front();
        }
    }
    f(entry)
}

fn notifier(engine: &Engine, hub: &HubInfo, partition: usize) -> Arc<Notify> {
    engine.state::<EventHubsState>().lock().notifier(&format!("{}/{partition}", hub.key))
}

/// The parts of an address within an Event Hubs namespace.
enum Address {
    Hub(String),
    Partition(String, usize),
    Receive { hub: String, group: String, partition: usize },
}

fn parse_address(path: &str) -> Option<Address> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let is = |s: &str, word: &str| s.eq_ignore_ascii_case(word);
    match parts.as_slice() {
        [hub] => Some(Address::Hub(hub.to_string())),
        [hub, p, n] if is(p, "partitions") => Some(Address::Partition(hub.to_string(), n.parse().ok()?)),
        [hub, c, group, p, n] if is(c, "consumergroups") && is(p, "partitions") => {
            Some(Address::Receive { hub: hub.to_string(), group: group.to_string(), partition: n.parse().ok()? })
        }
        _ => None,
    }
}

fn timestamp(time: SystemTime) -> Timestamp {
    Timestamp::from_milliseconds(time.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0))
}

fn annotation_text(message: &AmqpMessage, key: &str) -> Option<String> {
    let annotations = message.message_annotations.as_ref()?;
    match annotations.0.get(&fe2o3_amqp_types::messaging::annotations::OwnedKey::Symbol(Symbol::from(key)))? {
        Value::String(s) => Some(s.clone()),
        Value::Symbol(s) => Some(s.0.clone()),
        _ => None,
    }
}

fn set_annotation(message: &mut AmqpMessage, key: &str, value: Value) {
    let annotations = message.message_annotations.get_or_insert_with(|| MessageAnnotations(OrderedMap::new()));
    annotations.0.insert(fe2o3_amqp_types::messaging::annotations::OwnedKey::Symbol(Symbol::from(key)), value);
}

/// The partition a partition key maps to: stable for a key and a count.
fn partition_for_key(key: &str, count: usize) -> usize {
    // FNV-1a: any stable hash serves; Azure's own is not part of the contract.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    (hash % count as u64) as usize
}

/// Appends `messages` to one partition: the one named, the one their
/// partition key maps to, or the next in turn.
fn append(engine: &Engine, hub: &HubInfo, named: Option<usize>, messages: Vec<AmqpMessage>) -> Result<(), (&'static str, String)> {
    if let Some(n) = named {
        if n >= hub.partition_count {
            return Err(("amqp:not-found", format!("The partition '{n}' does not exist in the event hub '{}'.", hub.name)));
        }
    }
    let key = messages.first().and_then(|m| annotation_text(m, "x-opt-partition-key"));
    if named.is_some() && key.is_some() {
        return Err(("amqp:invalid-field", "A partition key cannot be used when sending to a specific partition.".into()));
    }
    for message in &messages {
        let size = store::encode(message.clone()).map(|b| b.len() as u64).unwrap_or(0);
        if size > MAX_EVENT_SIZE {
            return Err(("amqp:link:message-size-exceeded", format!("The event ({size} bytes) exceeds the maximum size ({MAX_EVENT_SIZE} bytes).")));
        }
    }
    let partition = with_hub(engine, hub, |h| {
        let index = match (named, &key) {
            (Some(n), _) => n,
            (None, Some(key)) => partition_for_key(key, hub.partition_count),
            (None, None) => {
                let n = h.next_partition % hub.partition_count;
                h.next_partition += 1;
                n
            }
        };
        let partition = &mut h.partitions[index];
        let now = SystemTime::now();
        for mut message in messages {
            let sequence = partition.next_sequence;
            let offset = partition.next_offset;
            set_annotation(&mut message, "x-opt-sequence-number", Value::Long(sequence));
            set_annotation(&mut message, "x-opt-offset", Value::String(offset.to_string()));
            set_annotation(&mut message, "x-opt-enqueued-time", Value::Timestamp(timestamp(now)));
            let size = store::encode(message.clone()).map(|b| b.len() as i64).unwrap_or(1);
            partition.events.push_back(Event { sequence, offset, enqueued: now, message });
            partition.next_sequence += 1;
            partition.next_offset += size.max(1);
        }
        index
    });
    notifier(engine, hub, partition).notify_waiters();
    Ok(())
}

/// A client's sender link to a hub or one of its partitions.
pub(super) async fn incoming(engine: Arc<Engine>, context: Arc<Context>, mut link: Receiver, namespace: String, path: String) {
    let (hub_name, named) = match parse_address(&path) {
        Some(Address::Hub(hub)) => (hub, None),
        Some(Address::Partition(hub, n)) => (hub, Some(n)),
        _ => {
            let _ = link.close_with_error(error("amqp:not-found", format!("'{path}' is not an event hub or a partition of one."))).await;
            return;
        }
    };
    let hub = match hub_info(&engine, &namespace, &hub_name) {
        Ok(hub) => hub,
        Err(e) => {
            let _ = link.close_with_error(error("amqp:not-found", e)).await;
            return;
        }
    };
    if !context.allows(&namespace, &path, super::Right::Send) {
        let _ = link.close_with_error(error("amqp:unauthorized-access", "Unauthorized access. 'Send' claim(s) are required to perform this operation.")).await;
        return;
    }
    while let Ok(delivery) = link.recv::<Body<Value>>().await {
        let batch = delivery.message_format() == &Some(super::BATCH_FORMAT);
        let info = DeliveryInfo::from(&delivery).settle_on_disposition();
        let message = delivery.into_message();
        let result = if batch {
            // A batch's partition key is on the batch envelope.
            let key = annotation_text(&message, "x-opt-partition-key");
            super::unbatch(message).map_err(|e| ("amqp:decode-error", e)).map(|mut events| {
                if let Some(key) = &key {
                    for event in &mut events {
                        set_annotation(event, "x-opt-partition-key", Value::String(key.clone()));
                    }
                }
                events
            })
        } else {
            Ok(vec![message])
        };
        let result = result.and_then(|events| if events.is_empty() { Ok(()) } else { append(&engine, &hub, named, events) });
        let _ = match result {
            Ok(()) => link.accept(info).await,
            Err((condition, description)) => link.reject(info, error(condition, description)).await,
        };
    }
}

/// Where a receiver starts: the first event after (or at) a position.
enum Start {
    Offset(i64, bool),
    Sequence(i64, bool),
    Enqueued(i64, bool),
    Earliest,
    Latest,
}

/// Parses `amqp.annotation.x-opt-offset > '123'` and its kin.
fn parse_selector(text: &str) -> Start {
    let text = text.trim();
    let Some(rest) = text.strip_prefix("amqp.annotation.x-opt-") else { return Start::Latest };
    let (field, rest) = rest.split_once(' ').unwrap_or((rest, ""));
    let rest = rest.trim();
    let (inclusive, value) = if let Some(v) = rest.strip_prefix(">=") {
        (true, v)
    } else if let Some(v) = rest.strip_prefix('>') {
        (false, v)
    } else if let Some(v) = rest.strip_prefix('=') {
        (true, v)
    } else {
        return Start::Latest;
    };
    let value = value.trim().trim_matches('\'');
    match (field, value) {
        ("offset", "-1") => Start::Earliest,
        ("offset", "@latest") => Start::Latest,
        ("offset", v) => v.parse().map_or(Start::Latest, |n| Start::Offset(n, inclusive)),
        ("sequence-number", v) => v.parse().map_or(Start::Latest, |n| Start::Sequence(n, inclusive)),
        ("enqueued-time", v) => v.parse().map_or(Start::Latest, |n| Start::Enqueued(n, inclusive)),
        _ => Start::Latest,
    }
}

/// What a receiver's attach asked for, read before the attach is answered.
pub(crate) struct ReceiverOptions {
    pub selector: Option<String>,
    pub epoch: Option<i64>,
}

impl ReceiverOptions {
    pub fn from_attach(attach: &fe2o3_amqp_types::performatives::Attach) -> Self {
        let selector = attach
            .source
            .as_ref()
            .and_then(|s| s.filter.as_ref())
            .and_then(|f| f.iter().find(|(k, _)| k.0 == "apache.org:selector-filter:string").map(|(_, v)| v.clone()))
            .and_then(|value| match value {
                Value::Described(described) => match described.value {
                    Value::String(s) => Some(s),
                    _ => None,
                },
                Value::String(s) => Some(s),
                _ => None,
            });
        let epoch = attach.properties.as_ref().and_then(|p| p.get(&Symbol::from("com.microsoft:epoch"))).and_then(|v| match v {
            Value::Long(n) => Some(*n),
            Value::Int(n) => Some(i64::from(*n)),
            Value::Ulong(n) => Some(*n as i64),
            _ => None,
        });
        ReceiverOptions { selector, epoch }
    }
}

/// A client's receiver link on one partition of one consumer group.
pub(super) async fn outgoing(engine: Arc<Engine>, context: Arc<Context>, mut link: Sender, namespace: String, path: String, options: ReceiverOptions) {
    let Some(Address::Receive { hub: hub_name, group, partition }) = parse_address(&path) else {
        let _ = link
            .close_with_error(error("amqp:not-found", format!("'{path}' is not a partition of a consumer group of an event hub.")))
            .await;
        return;
    };
    let hub = match hub_info(&engine, &namespace, &hub_name) {
        Ok(hub) if partition < hub.partition_count => hub,
        Ok(hub) => {
            let _ = link.close_with_error(error("amqp:not-found", format!("The partition '{partition}' does not exist in the event hub '{}'.", hub.name))).await;
            return;
        }
        Err(e) => {
            let _ = link.close_with_error(error("amqp:not-found", e)).await;
            return;
        }
    };
    if !consumer_group_exists(&engine, &hub, &group) {
        let _ = link.close_with_error(error("amqp:not-found", format!("The consumer group '{group}' does not exist in the event hub '{}'.", hub.name))).await;
        return;
    }
    if !context.allows(&namespace, &path, super::Right::Listen) {
        let _ = link.close_with_error(error("amqp:unauthorized-access", "Unauthorized access. 'Listen' claim(s) are required to perform this operation.")).await;
        return;
    }

    // Owner levels: a higher one takes the partition; a lower one is refused.
    let group_key = group.to_ascii_lowercase();
    let generation = with_hub(&engine, &hub, |h| {
        let slot = h.partitions[partition].owners.entry(group_key.clone()).or_insert((None, 0));
        match (options.epoch, slot.0) {
            (Some(mine), Some(held)) if mine < held => Err(format!("Receiver 'epoch {mine}' is lower than the current owner's ({held}).")),
            (None, Some(held)) => Err(format!("A receiver with owner level {held} holds the partition; one without an owner level cannot open.")),
            (Some(mine), _) => {
                *slot = (Some(mine), slot.1 + 1);
                Ok(slot.1)
            }
            (None, None) => Ok(slot.1),
        }
    });
    let generation = match generation {
        Ok(generation) => generation,
        Err(reason) => {
            let _ = link.close_with_error(error("com.microsoft:receiver-disconnected", reason)).await;
            return;
        }
    };

    let mut next_sequence = with_hub(&engine, &hub, |h| {
        let p = &h.partitions[partition];
        let first = p.events.front().map_or(p.next_sequence, |e| e.sequence);
        let after = |predicate: &dyn Fn(&Event) -> bool| p.events.iter().find(|e| predicate(e)).map_or(p.next_sequence, |e| e.sequence);
        match options.selector.as_deref().map(parse_selector).unwrap_or(Start::Latest) {
            Start::Earliest => first,
            Start::Latest => p.next_sequence,
            Start::Offset(o, inclusive) => after(&|e| if inclusive { e.offset >= o } else { e.offset > o }),
            Start::Sequence(s, inclusive) => (if inclusive { s } else { s + 1 }).max(first),
            Start::Enqueued(ms, inclusive) => after(&|e| {
                let at = e.enqueued.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
                if inclusive { at >= ms } else { at > ms }
            }),
        }
    });

    let notify = notifier(&engine, &hub, partition);
    loop {
        // Taken by a receiver with a higher owner level?
        let current = with_hub(&engine, &hub, |h| h.partitions[partition].owners.get(&group_key).map(|s| s.1).unwrap_or(0));
        if current != generation {
            let _ = link
                .close_with_error(error("amqp:link:stolen", "New receiver with higher epoch of '{}' is created hence current receiver is getting disconnected."))
                .await;
            return;
        }
        if tokio::time::timeout(Duration::from_millis(200), link.credit_available()).await.is_err() {
            if tokio::time::timeout(Duration::from_millis(1), link.on_detach()).await.is_ok() {
                release(&engine, &hub, partition, &group_key, generation, options.epoch);
                return;
            }
            continue;
        }
        let event = with_hub(&engine, &hub, |h| {
            let p = &h.partitions[partition];
            p.events.iter().find(|e| e.sequence >= next_sequence).map(|e| (e.sequence, e.message.clone()))
        });
        let Some((sequence, message)) = event else {
            tokio::select! {
                _ = notify.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                _ = link.on_detach() => {
                    release(&engine, &hub, partition, &group_key, generation, options.epoch);
                    return;
                }
            }
            continue;
        };
        let Ok(payload) = store::encode(message) else {
            next_sequence = sequence + 1;
            continue;
        };
        if link.send_encoded(payload, true, sequence.to_be_bytes().to_vec()).await.is_err() {
            release(&engine, &hub, partition, &group_key, generation, options.epoch);
            return;
        }
        next_sequence = sequence + 1;
    }
}

/// An owner-level receiver that ends gives the partition up.
fn release(engine: &Engine, hub: &HubInfo, partition: usize, group: &str, generation: u64, epoch: Option<i64>) {
    if epoch.is_none() {
        return;
    }
    with_hub(engine, hub, |h| {
        if let Some(slot) = h.partitions[partition].owners.get_mut(group) {
            if slot.1 == generation {
                slot.0 = None;
            }
        }
    });
}

fn text_property(request: &AmqpMessage, name: &str) -> Option<String> {
    match request.application_properties.as_ref()?.0.get(name)? {
        SimpleValue::String(s) => Some(s.clone()),
        SimpleValue::Symbol(s) => Some(s.0.clone()),
        _ => None,
    }
}

fn reply(request: &AmqpMessage, status: i32, description: &str, body: Value) -> AmqpMessage {
    let mut application = OrderedMap::new();
    application.insert("status-code".to_string(), SimpleValue::Int(status));
    application.insert("status-description".to_string(), SimpleValue::String(description.to_string()));
    Message {
        header: None,
        delivery_annotations: None,
        message_annotations: None,
        properties: Some(Properties { correlation_id: request.properties.as_ref().and_then(|p| p.message_id.clone()), ..Default::default() }),
        application_properties: Some(ApplicationProperties(application)),
        body: Body::Value(AmqpValue(body)),
        footer: None,
    }
}

/// Answers a request to an Event Hubs namespace's `$management` node.
pub(super) fn management(engine: &Engine, context: &Context, namespace: &str, request: &AmqpMessage) -> AmqpMessage {
    let operation = text_property(request, "operation").unwrap_or_default();
    if operation != "READ" {
        return reply(request, 501, &format!("The management operation '{operation}' is not implemented by the emulator."), Value::Null);
    }
    let Some(name) = text_property(request, "name") else {
        return reply(request, 400, "A READ needs the name of an event hub.", Value::Null);
    };
    let hub = match hub_info(engine, namespace, &name) {
        Ok(hub) => hub,
        Err(e) => return reply(request, 404, &e, Value::Null),
    };
    if !context.allows(namespace, &hub.name, super::Right::Listen) && !context.allows(namespace, &hub.name, super::Right::Send) {
        return reply(request, 401, "Unauthorized access. A claim on the event hub is required.", Value::Null);
    }
    let mut body = OrderedMap::new();
    let mut put = |key: &str, value: Value| {
        body.insert(Value::String(key.into()), value);
    };
    match text_property(request, "type").as_deref() {
        Some("com.microsoft:eventhub") => {
            put("name", Value::String(hub.name.clone()));
            put("type", Value::String("com.microsoft:eventhub".into()));
            put("created_at", Value::Timestamp(timestamp(hub.created)));
            put("partition_count", Value::Int(hub.partition_count as i32));
            put("partition_ids", Value::Array(Array::from((0..hub.partition_count).map(|i| Value::String(i.to_string())).collect::<Vec<_>>())));
        }
        Some("com.microsoft:partition") => {
            let Some(index) = text_property(request, "partition").and_then(|p| p.parse::<usize>().ok()).filter(|i| *i < hub.partition_count) else {
                return reply(request, 404, "The partition does not exist.", Value::Null);
            };
            let (begin, last_sequence, last_offset, last_time, empty) = with_hub(engine, &hub, |h| {
                let p = &h.partitions[index];
                match p.events.back() {
                    Some(last) => (p.events.front().map_or(0, |e| e.sequence), last.sequence, last.offset.to_string(), last.enqueued, false),
                    None => (p.next_sequence, p.next_sequence - 1, "-1".to_string(), UNIX_EPOCH, true),
                }
            });
            put("name", Value::String(hub.name.clone()));
            put("type", Value::String("com.microsoft:partition".into()));
            put("partition", Value::String(index.to_string()));
            put("begin_sequence_number", Value::Long(begin));
            put("last_enqueued_sequence_number", Value::Long(last_sequence));
            put("last_enqueued_offset", Value::String(last_offset));
            put("last_enqueued_time_utc", Value::Timestamp(timestamp(last_time)));
            put("is_partition_empty", Value::Bool(empty));
        }
        other => return reply(request, 400, &format!("'{}' is not a type READ knows.", other.unwrap_or_default()), Value::Null),
    }
    reply(request, 200, "OK", Value::Map(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors() {
        assert!(matches!(parse_selector("amqp.annotation.x-opt-offset > '-1'"), Start::Earliest));
        assert!(matches!(parse_selector("amqp.annotation.x-opt-offset > '@latest'"), Start::Latest));
        assert!(matches!(parse_selector("amqp.annotation.x-opt-offset >= '42'"), Start::Offset(42, true)));
        assert!(matches!(parse_selector("amqp.annotation.x-opt-sequence-number > '7'"), Start::Sequence(7, false)));
        assert!(matches!(parse_selector("amqp.annotation.x-opt-enqueued-time > '1700000000000'"), Start::Enqueued(1_700_000_000_000, false)));
    }

    #[test]
    fn partition_keys_are_stable() {
        assert_eq!(partition_for_key("tenant-a", 4), partition_for_key("tenant-a", 4));
        assert!(partition_for_key("tenant-b", 4) < 4);
    }

    #[test]
    fn addresses() {
        assert!(matches!(parse_address("hub"), Some(Address::Hub(h)) if h == "hub"));
        assert!(matches!(parse_address("hub/Partitions/2"), Some(Address::Partition(_, 2))));
        assert!(matches!(parse_address("hub/ConsumerGroups/$Default/Partitions/0"), Some(Address::Receive { partition: 0, .. })));
        assert!(parse_address("hub/x").is_none());
    }
}
