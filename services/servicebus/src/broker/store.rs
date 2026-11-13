//! The broker's message store: what is in each queue and subscription, and what
//! sending, receiving and settling a message does to it.
//!
//! Entities are the ones Resource Manager holds: a queue or subscription exists
//! for the broker exactly when its resource does, and its settings (lock
//! duration, maximum delivery count, time to live, dead-lettering on expiry,
//! duplicate detection, status, auto-forwarding) are read from the resource on
//! every operation, so changing them takes effect at once.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use emulator_core::arm::{ArmState, Resource};
use emulator_core::Engine;
use fe2o3_amqp_types::messaging::annotations::OwnedKey;
use fe2o3_amqp_types::messaging::{
    ApplicationProperties, Body, Header, Message, MessageAnnotations, MessageId,
};
use fe2o3_amqp_types::primitives::{SimpleValue, Symbol, Timestamp, Value};
use serde_json::Value as Json;
use tokio::sync::Notify;

use super::duration;

pub(crate) type AmqpMessage = Message<Body<Value>>;

/// The maximum message size of a Standard namespace: 256 KB.
pub(crate) const DEFAULT_MAX_MESSAGE_SIZE: u64 = 256 * 1024;

const NAMESPACE_TYPE: &str = "Microsoft.ServiceBus/namespaces";

/// Why an operation on an entity was refused, as an AMQP error condition.
#[derive(Debug, Clone)]
pub(crate) struct Refusal {
    pub condition: &'static str,
    pub description: String,
}

pub(crate) fn refuse(condition: &'static str, description: impl Into<String>) -> Refusal {
    Refusal {
        condition,
        description: description.into(),
    }
}

fn not_found(what: impl std::fmt::Display) -> Refusal {
    refuse("amqp:not-found", format!("The messaging entity '{what}' could not be found."))
}

/// What kind of entity an address names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Queue,
    Topic,
    Subscription,
}

/// An entity resolved from an address, with its settings.
#[derive(Debug, Clone)]
pub(crate) struct Entity {
    /// Store key: lowercased `namespace/path`, with `/$deadletterqueue` for a
    /// dead-letter subqueue.
    pub key: String,
    pub namespace: String,
    /// The entity's path within its namespace, as addressed (`orders`,
    /// `events/Subscriptions/audit`).
    pub path: String,
    pub kind: Kind,
    pub dead_letter: bool,
    /// The resource the settings come from (the queue, topic or subscription).
    pub resource: Resource,
}

impl Entity {
    fn setting(&self, name: &str) -> Option<&Json> {
        self.resource.properties.get(name)
    }

    fn duration_setting(&self, name: &str) -> Option<Duration> {
        self.setting(name).and_then(Json::as_str).and_then(duration::parse)
    }

    pub fn lock_duration(&self) -> Duration {
        self.duration_setting("lockDuration").unwrap_or(Duration::from_secs(60))
    }

    fn max_delivery_count(&self) -> u32 {
        self.setting("maxDeliveryCount").and_then(Json::as_u64).unwrap_or(10) as u32
    }

    fn default_ttl(&self) -> Option<Duration> {
        self.duration_setting("defaultMessageTimeToLive")
    }

    fn dead_letter_on_expiry(&self) -> bool {
        self.setting("deadLetteringOnMessageExpiration").and_then(Json::as_bool).unwrap_or(false)
    }

    /// The largest message the entity accepts, in bytes.
    pub fn max_message_size(&self) -> u64 {
        self.setting("maxMessageSizeInKilobytes").and_then(Json::as_u64).map_or(DEFAULT_MAX_MESSAGE_SIZE, |kb| kb * 1024)
    }

    pub fn requires_session(&self) -> bool {
        self.setting("requiresSession").and_then(Json::as_bool).unwrap_or(false)
    }

    fn status(&self) -> &str {
        self.setting("status").and_then(Json::as_str).unwrap_or("Active")
    }

    /// Refuses sending when the entity's status does not allow it.
    pub fn check_send(&self) -> Result<(), Refusal> {
        match self.status() {
            "Disabled" | "SendDisabled" => Err(refuse(
                "com.microsoft:entity-disabled",
                format!("Messaging entity '{}' is currently disabled for sending.", self.path),
            )),
            _ => Ok(()),
        }
    }

    /// Refuses receiving when the entity's status does not allow it.
    pub fn check_receive(&self) -> Result<(), Refusal> {
        match self.status() {
            "Disabled" | "ReceiveDisabled" => Err(refuse(
                "com.microsoft:entity-disabled",
                format!("Messaging entity '{}' is currently disabled for receiving.", self.path),
            )),
            _ => Ok(()),
        }
    }
}

/// The namespace resource called `name`, if there is one.
fn namespace_resource(state: &ArmState, name: &str) -> Option<Resource> {
    state
        .resources()
        .find(|r| r.resource_type.eq_ignore_ascii_case(NAMESPACE_TYPE) && r.name.eq_ignore_ascii_case(name))
        .cloned()
}

/// Resolves `path` (an entity path, with any `$DeadLetterQueue` suffix) in
/// `namespace`.
pub(crate) fn resolve(engine: &Engine, namespace: &str, path: &str) -> Result<Entity, Refusal> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let lower: Vec<String> = segments.iter().map(|s| s.to_ascii_lowercase()).collect();
    let (dead_letter, entity_segments) = match lower.last().map(String::as_str) {
        Some("$deadletterqueue") => (true, &segments[..segments.len() - 1]),
        _ => (false, &segments[..]),
    };
    let state = engine.state::<ArmState>();
    let state = state.lock();
    let ns = namespace_resource(&state, namespace).ok_or_else(|| not_found(format!("{namespace}/{path}")))?;
    let (kind, id) = match entity_segments {
        [name] => {
            let queue = format!("{}/queues/{name}", ns.id);
            let topic = format!("{}/topics/{name}", ns.id);
            if state.resource(&queue).is_some() {
                (Kind::Queue, queue)
            } else if state.resource(&topic).is_some() && !dead_letter {
                (Kind::Topic, topic)
            } else {
                return Err(not_found(format!("{}/{path}", ns.name)));
            }
        }
        [topic, middle, subscription] if middle.eq_ignore_ascii_case("subscriptions") => {
            (Kind::Subscription, format!("{}/topics/{topic}/subscriptions/{subscription}", ns.id))
        }
        _ => return Err(not_found(format!("{}/{path}", ns.name))),
    };
    let resource = state.resource(&id).cloned().ok_or_else(|| not_found(format!("{}/{path}", ns.name)))?;
    let base = entity_segments.join("/");
    let mut key = format!("{}/{}", ns.name, base).to_ascii_lowercase();
    if dead_letter {
        key.push_str("/$deadletterqueue");
    }
    Ok(Entity {
        key,
        namespace: ns.name.to_ascii_lowercase(),
        path: path.to_string(),
        kind,
        dead_letter,
        resource,
    })
}

/// Where a message is in its life.
#[derive(Debug, Clone, PartialEq)]
enum Status {
    Available,
    /// Handed to a receiver, waiting for it to be sent (not yet locked).
    Reserved,
    Locked { token: uuid::Uuid, until: SystemTime },
    Deferred,
}

#[derive(Debug, Clone)]
struct Stored {
    message: AmqpMessage,
    sequence: i64,
    enqueued: SystemTime,
    /// Not receivable before this (a scheduled message).
    available_at: SystemTime,
    expires: Option<SystemTime>,
    /// Prior delivery attempts.
    delivery_count: u32,
    status: Status,
    /// For a dead-lettered message: the entity it came from.
    dead_letter_source: Option<String>,
}

#[derive(Default)]
struct Queue {
    messages: BTreeMap<i64, Stored>,
    /// Message ids seen, for duplicate detection.
    seen: HashMap<String, SystemTime>,
}

/// One message session of a session-enabled entity.
#[derive(Default)]
struct Session {
    /// The link holding the session lock, and until when.
    lock: Option<(u64, SystemTime)>,
    state: Option<Vec<u8>>,
}

/// Every entity's messages, and the sequence counters.
#[derive(Default)]
pub struct BrokerState {
    queues: HashMap<String, Queue>,
    /// Sessions per entity key, by session id.
    sessions: HashMap<String, HashMap<String, Session>>,
    /// Next sequence number per entity that assigns them (queues and topics).
    sequences: HashMap<String, i64>,
    notifiers: HashMap<String, Arc<Notify>>,
}

fn symbol(key: &str) -> OwnedKey {
    OwnedKey::Symbol(Symbol::from(key))
}

fn timestamp(time: SystemTime) -> Timestamp {
    let millis = time.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    Timestamp::from_milliseconds(millis)
}

fn annotation<'a>(message: &'a AmqpMessage, key: &str) -> Option<&'a Value> {
    message.message_annotations.as_ref().and_then(|a| a.0.get(&symbol(key)))
}

fn set_annotation(message: &mut AmqpMessage, key: &str, value: Value) {
    message
        .message_annotations
        .get_or_insert_with(|| MessageAnnotations(Default::default()))
        .0
        .insert(symbol(key), value);
}

fn set_application_property(message: &mut AmqpMessage, key: &str, value: SimpleValue) {
    message
        .application_properties
        .get_or_insert_with(|| ApplicationProperties(Default::default()))
        .0
        .insert(key.to_string(), value);
}

fn message_id_text(message: &AmqpMessage) -> Option<String> {
    match message.properties.as_ref()?.message_id.as_ref()? {
        MessageId::String(text) => Some(text.clone()),
        MessageId::Ulong(number) => Some(number.to_string()),
        MessageId::Uuid(uuid) => Some(uuid::Uuid::from_bytes(*uuid.as_inner()).to_string()),
        MessageId::Binary(bytes) => Some(format!("{bytes:?}")),
    }
}

impl BrokerState {
    pub(crate) fn notifier(&mut self, key: &str) -> Arc<Notify> {
        self.notifiers.entry(key.to_string()).or_insert_with(|| Arc::new(Notify::new())).clone()
    }

    fn next_sequence(&mut self, key: &str) -> i64 {
        let next = self.sequences.entry(key.to_string()).or_insert(1);
        let sequence = *next;
        *next += 1;
        sequence
    }

    fn store(&mut self, key: &str, stored: Stored) {
        self.queues.entry(key.to_string()).or_default().messages.insert(stored.sequence, stored);
        self.notifier(key).notify_waiters();
    }

}

/// An entity's message counts for Resource Manager, by its broker key.
pub(crate) fn counts(engine: &Engine, key: &str) -> serde_json::Value {
    let now = SystemTime::now();
    let broker = engine.state::<BrokerState>();
    let broker = broker.lock();
    let (mut active, mut scheduled) = (0usize, 0usize);
    for message in broker.queues.get(key).map(|q| q.messages.values()).into_iter().flatten() {
        if message.available_at > now {
            scheduled += 1;
        } else {
            active += 1;
        }
    }
    let dead = broker.queues.get(&format!("{key}/$deadletterqueue")).map_or(0, |q| q.messages.len());
    serde_json::json!({
        "messageCount": active + scheduled + dead,
        "countDetails": {
            "activeMessageCount": active,
            "deadLetterMessageCount": dead,
            "scheduledMessageCount": scheduled,
            "transferMessageCount": 0,
            "transferDeadLetterMessageCount": 0,
        },
    })
}

/// Accepts `message` sent to `entity`: a queue (or a forwarding chain from it),
/// or a topic, which copies it to every subscription whose rules match.
pub(crate) fn send(engine: &Engine, entity: &Entity, mut message: AmqpMessage) -> Result<i64, Refusal> {
    entity.check_send()?;
    // Service Bus gives a message without a message id one of its own.
    let properties = message.properties.get_or_insert_with(Default::default);
    if properties.message_id.is_none() {
        properties.message_id = Some(MessageId::String(uuid::Uuid::new_v4().simple().to_string()));
    }
    if entity.dead_letter {
        return Err(refuse("amqp:not-allowed", "Messages cannot be sent to a dead-letter queue."));
    }
    if entity.kind == Kind::Subscription {
        return Err(refuse("amqp:not-allowed", "Messages are sent to a topic, not to one of its subscriptions."));
    }
    let max_bytes = entity.max_message_size();
    let size = serde_amqp::to_vec(&fe2o3_amqp_types::messaging::message::__private::Serializable(&message))
        .map(|bytes| bytes.len())
        .unwrap_or(0);
    if size as u64 > max_bytes {
        return Err(refuse(
            "amqp:link:message-size-exceeded",
            format!("The message ({size} bytes) exceeds the entity's maximum message size ({max_bytes} bytes)."),
        ));
    }

    let now = SystemTime::now();
    let broker = engine.state::<BrokerState>();
    let mut broker = broker.lock();

    // Duplicate detection: a message id seen within the window is dropped.
    if entity.setting("requiresDuplicateDetection").and_then(Json::as_bool) == Some(true) {
        if let Some(id) = message_id_text(&message) {
            let window = entity.duration_setting("duplicateDetectionHistoryTimeWindow").unwrap_or(Duration::from_secs(600));
            let queue = broker.queues.entry(entity.key.clone()).or_default();
            queue.seen.retain(|_, at| now.duration_since(*at).unwrap_or_default() < window);
            if queue.seen.contains_key(&id) {
                return Ok(0);
            }
            queue.seen.insert(id, now);
        }
    }

    let sequence = broker.next_sequence(&entity.key);
    let available_at = match annotation(&message, "x-opt-scheduled-enqueue-time") {
        Some(Value::Timestamp(at)) => SystemTime::UNIX_EPOCH + Duration::from_millis(at.milliseconds().max(0) as u64),
        _ => now,
    };
    let targets = match entity.kind {
        Kind::Queue => vec![(entity.clone(), message.clone())],
        Kind::Topic => matching_subscriptions(engine, entity, &message)?,
        Kind::Subscription => Vec::new(),
    };
    let mut filled = std::collections::HashSet::new();
    for (target, message) in &targets {
        if target.requires_session() && session_of(message).is_none() {
            return Err(refuse(
                "amqp:not-allowed",
                format!(
                    "The SessionId was not set on a message, and it cannot be sent to the entity '{}'. Entities that have session support enabled can only receive messages that have the SessionId set.",
                    target.path
                ),
            ));
        }
    }
    for (target, message) in targets {
        // A second copy for the same subscription (another rule's action)
        // needs a sequence number of its own.
        let sequence = if filled.insert(target.key.clone()) { sequence } else { broker.next_sequence(&entity.key) };
        let target = forwarded(engine, target)?;
        let message_ttl = message
            .header
            .as_ref()
            .and_then(|h| h.ttl)
            .map(|ms| Duration::from_millis(ms as u64));
        let ttl = match (message_ttl, target.default_ttl()) {
            (Some(own), Some(default)) => Some(own.min(default)),
            (own, default) => own.or(default),
        };
        let stored = Stored {
            message,
            sequence,
            enqueued: now,
            available_at,
            expires: ttl.and_then(|ttl| available_at.checked_add(ttl)),
            delivery_count: 0,
            status: Status::Available,
            dead_letter_source: None,
        };
        broker.store(&target.key, stored);
    }
    Ok(sequence)
}

/// Follows a queue's or subscription's `forwardTo` chain to where messages end up.
fn forwarded(engine: &Engine, mut entity: Entity) -> Result<Entity, Refusal> {
    for _ in 0..4 {
        let Some(next) = entity.setting("forwardTo").and_then(Json::as_str).filter(|n| !n.is_empty()) else {
            return Ok(entity);
        };
        entity = resolve(engine, &entity.namespace, next)?;
        if entity.kind == Kind::Topic {
            return Err(refuse("amqp:not-implemented", "Auto-forwarding to a topic is not supported by the emulator yet."));
        }
    }
    Err(refuse("amqp:not-allowed", "The auto-forwarding chain is longer than four hops."))
}

/// The copies of `message` the subscriptions of `topic` receive. A
/// subscription gets one unmodified copy if any matching rule has no action,
/// and one copy per matching rule that has an action, with that action applied.
fn matching_subscriptions(engine: &Engine, topic: &Entity, message: &AmqpMessage) -> Result<Vec<(Entity, AmqpMessage)>, Refusal> {
    let subscriptions: Vec<(Resource, Vec<Resource>)> = {
        let state = engine.state::<ArmState>();
        let state = state.lock();
        state
            .children(&format!("{}/subscriptions", topic.resource.id))
            .map(|sub| (sub.clone(), state.children(&format!("{}/rules", sub.id)).cloned().collect()))
            .collect()
    };
    let mut out = Vec::new();
    for (subscription, rules) in subscriptions {
        let mut copies = Vec::new();
        let mut plain = false;
        for rule in &rules {
            if !super::filters::matches(&rule.properties, message)? {
                continue;
            }
            match super::filters::action(&rule.properties)? {
                Some(action) => {
                    let mut copy = message.clone();
                    action.apply(&mut copy);
                    copies.push(copy);
                }
                None => plain = true,
            }
        }
        if plain {
            copies.insert(0, message.clone());
        }
        let target = Entity {
            key: format!("{}/{}/subscriptions/{}", topic.namespace, topic.resource.name, subscription.name).to_ascii_lowercase(),
            namespace: topic.namespace.clone(),
            path: format!("{}/Subscriptions/{}", topic.resource.name, subscription.name),
            kind: Kind::Subscription,
            dead_letter: false,
            resource: subscription,
        };
        out.extend(copies.into_iter().map(|copy| (target.clone(), copy)));
    }
    Ok(out)
}

/// A message handed to a receiver.
pub(crate) struct Delivery {
    pub sequence: i64,
    pub lock_token: uuid::Uuid,
    pub message: AmqpMessage,
}

/// Moves a stored message to the entity's dead-letter subqueue.
fn dead_letter(broker: &mut BrokerState, key: &str, mut stored: Stored, reason: &str, description: &str) {
    set_application_property(&mut stored.message, "DeadLetterReason", SimpleValue::String(reason.into()));
    set_application_property(&mut stored.message, "DeadLetterErrorDescription", SimpleValue::String(description.into()));
    stored.status = Status::Available;
    stored.dead_letter_source = Some(key.to_string());
    stored.available_at = SystemTime::now();
    stored.expires = None;
    let dlq = format!("{key}/$deadletterqueue");
    stored.sequence = broker.next_sequence(&dlq);
    broker.store(&dlq, stored);
}

/// Applies what time has done: expired locks return messages (counting the
/// attempt), and expired messages are dead-lettered or dropped.
fn sweep(broker: &mut BrokerState, entity: &Entity, now: SystemTime) {
    let max = entity.max_delivery_count();
    let mut expired = Vec::new();
    let mut exhausted = Vec::new();
    if let Some(queue) = broker.queues.get_mut(&entity.key) {
        for (sequence, stored) in queue.messages.iter_mut() {
            if let Status::Locked { until, .. } = stored.status {
                if now >= until {
                    stored.status = Status::Available;
                    stored.delivery_count += 1;
                    if stored.delivery_count >= max && !entity.dead_letter {
                        exhausted.push(*sequence);
                    }
                }
            }
            if stored.status == Status::Available && stored.expires.is_some_and(|at| now >= at) {
                expired.push(*sequence);
            }
        }
    }
    for sequence in exhausted {
        if let Some(stored) = broker.queues.get_mut(&entity.key).and_then(|q| q.messages.remove(&sequence)) {
            dead_letter(broker, &entity.key, stored, "MaxDeliveryCountExceeded", "Message could not be consumed after the maximum number of delivery attempts.");
        }
    }
    for sequence in expired {
        let Some(stored) = broker.queues.get_mut(&entity.key).and_then(|q| q.messages.remove(&sequence)) else {
            continue;
        };
        if entity.dead_letter_on_expiry() && !entity.dead_letter {
            dead_letter(broker, &entity.key, stored, "TTLExpiredException", "The message expired and was dead lettered.");
        }
    }
}

/// The message with sequence number `sequence` in its delivered form.
fn delivered(stored: &Stored, locked_until: Option<SystemTime>) -> AmqpMessage {
    let mut message = stored.message.clone();
    let header = message.header.get_or_insert_with(Header::default);
    header.delivery_count = stored.delivery_count;
    set_annotation(&mut message, "x-opt-sequence-number", Value::Long(stored.sequence));
    set_annotation(&mut message, "x-opt-enqueued-time", Value::Timestamp(timestamp(stored.enqueued)));
    if let Some(until) = locked_until {
        set_annotation(&mut message, "x-opt-locked-until", Value::Timestamp(timestamp(until)));
    }
    if let Some(source) = &stored.dead_letter_source {
        set_annotation(&mut message, "x-opt-deadletter-source", Value::String(source.clone()));
    }
    message
}

/// The session a message belongs to.
pub(crate) fn session_of(message: &AmqpMessage) -> Option<&str> {
    message.properties.as_ref().and_then(|p| p.group_id.as_deref())
}

/// Takes the next receivable message from `entity`: reserved (for peek-lock,
/// locked once [`lock`] is called) or removed (receive-and-delete).
pub(crate) fn take(engine: &Engine, entity: &Entity, remove: bool, session: Option<&str>) -> Option<Delivery> {
    let now = SystemTime::now();
    let broker = engine.state::<BrokerState>();
    let mut broker = broker.lock();
    sweep(&mut broker, entity, now);
    let queue = broker.queues.get_mut(&entity.key)?;
    let sequence = *queue
        .messages
        .iter()
        .find(|(_, m)| {
            m.status == Status::Available && m.available_at <= now && (session.is_none() || session_of(&m.message) == session)
        })?
        .0;
    if remove {
        let stored = queue.messages.remove(&sequence)?;
        return Some(Delivery {
            sequence,
            lock_token: uuid::Uuid::nil(),
            message: delivered(&stored, None),
        });
    }
    let stored = queue.messages.get_mut(&sequence)?;
    stored.status = Status::Reserved;
    let lock_token = uuid::Uuid::new_v4();
    let until = now + entity.lock_duration();
    Some(Delivery {
        sequence,
        lock_token,
        message: delivered(stored, Some(until)),
    })
}

/// Locks a reserved message once it has been sent: the lock runs from now.
pub(crate) fn lock(engine: &Engine, entity: &Entity, sequence: i64, token: uuid::Uuid) -> SystemTime {
    let until = SystemTime::now() + entity.lock_duration();
    let broker = engine.state::<BrokerState>();
    let mut broker = broker.lock();
    if let Some(stored) = broker.queues.get_mut(&entity.key).and_then(|q| q.messages.get_mut(&sequence)) {
        if stored.status == Status::Reserved {
            stored.status = Status::Locked { token, until };
        }
    }
    until
}

/// Returns a reserved message that could not be sent.
pub(crate) fn unreserve(engine: &Engine, entity: &Entity, sequence: i64) {
    let broker = engine.state::<BrokerState>();
    let mut broker = broker.lock();
    if let Some(stored) = broker.queues.get_mut(&entity.key).and_then(|q| q.messages.get_mut(&sequence)) {
        if stored.status == Status::Reserved {
            stored.status = Status::Available;
        }
    }
    broker.notifier(&entity.key).notify_waiters();
}

/// What a receiver did with a peek-locked message.
pub(crate) enum Settlement {
    Complete,
    /// Return it to the queue; `failed` counts it as a delivery attempt.
    Abandon { failed: bool, properties: Vec<(String, SimpleValue)> },
    Defer { properties: Vec<(String, SimpleValue)> },
    DeadLetter { reason: String, description: String, properties: Vec<(String, SimpleValue)> },
}

/// Applies `settlement` to the message locked with `token`. A lock that has
/// expired (or been taken by another receiver since) is lost: nothing happens.
pub(crate) fn settle(engine: &Engine, entity: &Entity, sequence: i64, token: uuid::Uuid, settlement: Settlement) -> bool {
    let now = SystemTime::now();
    let broker = engine.state::<BrokerState>();
    let mut broker = broker.lock();
    let Some(queue) = broker.queues.get_mut(&entity.key) else {
        return false;
    };
    let held = queue
        .messages
        .get(&sequence)
        .is_some_and(|m| matches!(m.status, Status::Locked { token: held, until } if held == token && now < until));
    if !held {
        return false;
    }
    let mut stored = queue.messages.remove(&sequence).expect("checked above");
    let apply = |stored: &mut Stored, properties: Vec<(String, SimpleValue)>| {
        for (key, value) in properties {
            set_application_property(&mut stored.message, &key, value);
        }
    };
    match settlement {
        Settlement::Complete => {}
        Settlement::Abandon { failed, properties } => {
            apply(&mut stored, properties);
            stored.status = Status::Available;
            if failed {
                stored.delivery_count += 1;
            }
            if stored.delivery_count >= entity.max_delivery_count() && !entity.dead_letter {
                dead_letter(&mut broker, &entity.key, stored, "MaxDeliveryCountExceeded", "Message could not be consumed after the maximum number of delivery attempts.");
            } else {
                broker.store(&entity.key, stored);
            }
        }
        Settlement::Defer { properties } => {
            apply(&mut stored, properties);
            stored.status = Status::Deferred;
            broker.store(&entity.key, stored);
        }
        Settlement::DeadLetter { reason, description, properties } => {
            apply(&mut stored, properties);
            if entity.dead_letter {
                // A dead-lettered message cannot be dead-lettered again; it stays.
                stored.status = Status::Available;
                broker.store(&entity.key, stored);
            } else {
                dead_letter(&mut broker, &entity.key, stored, &reason, &description);
            }
        }
    }
    true
}

/// Renews the lock `token` holds on a message; the new expiry, or `None` if
/// the lock is lost.
pub(crate) fn renew(engine: &Engine, entity: &Entity, token: uuid::Uuid) -> Option<SystemTime> {
    let now = SystemTime::now();
    let broker = engine.state::<BrokerState>();
    let mut broker = broker.lock();
    let queue = broker.queues.get_mut(&entity.key)?;
    let stored = queue
        .messages
        .values_mut()
        .find(|m| matches!(m.status, Status::Locked { token: held, until } if held == token && now < until))?;
    let until = now + entity.lock_duration();
    stored.status = Status::Locked { token, until };
    Some(until)
}

/// Up to `count` messages from sequence number `from`, in any state, without
/// locking them: what peeking sees.
pub(crate) fn peek(engine: &Engine, entity: &Entity, from: i64, count: usize, session: Option<&str>) -> Vec<AmqpMessage> {
    let now = SystemTime::now();
    let broker = engine.state::<BrokerState>();
    let mut broker = broker.lock();
    sweep(&mut broker, entity, now);
    broker
        .queues
        .get(&entity.key)
        .map(|q| {
            q.messages
                .range(from..)
                .filter(|(_, m)| session.is_none() || session_of(&m.message) == session)
                .take(count)
                .map(|(_, m)| delivered(m, None))
                .collect()
        })
        .unwrap_or_default()
}

/// Drops everything stored for entities that no longer exist under `prefix`
/// (a deleted queue, topic, subscription or namespace).
pub(crate) fn forget(engine: &Engine, prefix: &str) {
    let prefix = prefix.to_ascii_lowercase();
    let broker = engine.state::<BrokerState>();
    let mut broker = broker.lock();
    let gone = |key: &String| key == &prefix || key.starts_with(&format!("{prefix}/"));
    broker.queues.retain(|key, _| !gone(key));
    broker.sequences.retain(|key, _| !gone(key));
    broker.sessions.retain(|key, _| !gone(key));
}

/// The sequence number of the message locked with `token`, if the lock holds.
pub(crate) fn locked_sequence(engine: &Engine, entity: &Entity, token: uuid::Uuid) -> Option<i64> {
    let now = SystemTime::now();
    let broker = engine.state::<BrokerState>();
    let broker = broker.lock();
    broker.queues.get(&entity.key)?.messages.iter().find_map(|(sequence, m)| {
        matches!(m.status, Status::Locked { token: held, until } if held == token && now < until).then_some(*sequence)
    })
}

/// Hands out deferred messages by sequence number: locked (with their lock
/// tokens) or, for receive-and-delete, removed. Every number must name a
/// deferred message, or nothing is handed out.
pub(crate) fn receive_deferred(engine: &Engine, entity: &Entity, sequences: &[i64], remove: bool) -> Result<Vec<(uuid::Uuid, AmqpMessage)>, Refusal> {
    let now = SystemTime::now();
    let broker = engine.state::<BrokerState>();
    let mut broker = broker.lock();
    let queue = broker.queues.entry(entity.key.clone()).or_default();
    if let Some(missing) = sequences.iter().find(|s| !queue.messages.get(s).is_some_and(|m| m.status == Status::Deferred)) {
        return Err(refuse("com.microsoft:message-not-found", format!("Failed to lock one or more specified messages. The message with sequence number {missing} was not found or is not deferred.")));
    }
    let mut out = Vec::new();
    for sequence in sequences {
        if remove {
            let stored = queue.messages.remove(sequence).expect("checked above");
            out.push((uuid::Uuid::nil(), delivered(&stored, None)));
        } else {
            let stored = queue.messages.get_mut(sequence).expect("checked above");
            let token = uuid::Uuid::new_v4();
            let until = now + entity.lock_duration();
            stored.status = Status::Locked { token, until };
            out.push((token, delivered(stored, Some(until))));
        }
    }
    Ok(out)
}

/// Removes scheduled messages that have not been enqueued yet.
pub(crate) fn cancel_scheduled(engine: &Engine, entity: &Entity, sequences: &[i64]) {
    let now = SystemTime::now();
    let broker = engine.state::<BrokerState>();
    let mut broker = broker.lock();
    if let Some(queue) = broker.queues.get_mut(&entity.key) {
        for sequence in sequences {
            if queue.messages.get(sequence).is_some_and(|m| m.status == Status::Available && m.available_at > now) {
                queue.messages.remove(sequence);
            }
        }
    }
}


/// Why a session could not be locked.
pub(crate) enum SessionRefusal {
    /// Another receiver holds it.
    Locked(Refusal),
    /// No session has messages to receive right now.
    NoneAvailable,
}

/// Locks a session of `entity` for the link `owner`: the one `requested`, or
/// else the session of the oldest receivable message whose session is free.
/// Returns the session id and when the lock expires.
pub(crate) fn acquire_session(
    engine: &Engine,
    entity: &Entity,
    requested: Option<&str>,
    owner: u64,
) -> Result<(String, SystemTime), SessionRefusal> {
    let now = SystemTime::now();
    let until = now + entity.lock_duration();
    let broker = engine.state::<BrokerState>();
    let mut broker = broker.lock();
    sweep(&mut broker, entity, now);
    let free = |sessions: Option<&HashMap<String, Session>>, id: &str| {
        sessions.and_then(|s| s.get(id)).and_then(|s| s.lock).is_none_or(|(holder, until)| holder == owner || now >= until)
    };
    let id = match requested {
        Some(id) => {
            if !free(broker.sessions.get(&entity.key), id) {
                return Err(SessionRefusal::Locked(refuse(
                    "com.microsoft:session-cannot-be-locked",
                    format!("The requested session '{id}' cannot be accepted. It may be locked by another receiver."),
                )));
            }
            id.to_string()
        }
        None => {
            let sessions = broker.sessions.get(&entity.key);
            broker
                .queues
                .get(&entity.key)
                .and_then(|q| {
                    q.messages
                        .values()
                        .filter(|m| m.status == Status::Available && m.available_at <= now)
                        .filter_map(|m| session_of(&m.message))
                        .find(|id| free(sessions, id))
                        .map(str::to_string)
                })
                .ok_or(SessionRefusal::NoneAvailable)?
        }
    };
    broker.sessions.entry(entity.key.clone()).or_default().entry(id.clone()).or_default().lock = Some((owner, until));
    Ok((id, until))
}

/// Whether the link `owner` still holds the lock on session `id`.
pub(crate) fn holds_session(engine: &Engine, entity: &Entity, id: &str, owner: u64) -> bool {
    let now = SystemTime::now();
    let broker = engine.state::<BrokerState>();
    let broker = broker.lock();
    broker
        .sessions
        .get(&entity.key)
        .and_then(|s| s.get(id))
        .and_then(|s| s.lock)
        .is_some_and(|(holder, until)| holder == owner && now < until)
}

/// Releases the lock the link `owner` holds on session `id`.
pub(crate) fn release_session(engine: &Engine, entity: &Entity, id: &str, owner: u64) {
    let broker = engine.state::<BrokerState>();
    let mut broker = broker.lock();
    if let Some(session) = broker.sessions.get_mut(&entity.key).and_then(|s| s.get_mut(id)) {
        if session.lock.is_some_and(|(holder, _)| holder == owner) {
            session.lock = None;
        }
    }
    broker.notifier(&entity.key).notify_waiters();
}

/// Renews a held session lock; its new expiry, or `None` if it is not held.
pub(crate) fn renew_session(engine: &Engine, entity: &Entity, id: &str) -> Option<SystemTime> {
    let now = SystemTime::now();
    let broker = engine.state::<BrokerState>();
    let mut broker = broker.lock();
    let session = broker.sessions.get_mut(&entity.key)?.get_mut(id)?;
    let (holder, until) = session.lock?;
    if now >= until {
        return None;
    }
    let until = now + entity.lock_duration();
    session.lock = Some((holder, until));
    Some(until)
}

/// A session's state, if it is locked (by anyone).
pub(crate) fn session_state(engine: &Engine, entity: &Entity, id: &str) -> Option<Option<Vec<u8>>> {
    let now = SystemTime::now();
    let broker = engine.state::<BrokerState>();
    let broker = broker.lock();
    let session = broker.sessions.get(&entity.key)?.get(id)?;
    session.lock.filter(|(_, until)| now < *until)?;
    Some(session.state.clone())
}

/// Sets a locked session's state; false if the session is not locked.
pub(crate) fn set_session_state(engine: &Engine, entity: &Entity, id: &str, state: Option<Vec<u8>>) -> bool {
    let now = SystemTime::now();
    let broker = engine.state::<BrokerState>();
    let mut broker = broker.lock();
    match broker.sessions.get_mut(&entity.key).and_then(|s| s.get_mut(id)) {
        Some(session) if session.lock.is_some_and(|(_, until)| now < until) => {
            session.state = state;
            true
        }
        _ => false,
    }
}

/// Encodes a message for the wire. The header is written by hand with every
/// field present: the derived encoding leaves out fields at their defaults,
/// and the SDKs read a missing `delivery-count` as "unknown" rather than 0.
pub(crate) fn encode(mut message: AmqpMessage) -> Result<bytes::Bytes, serde_amqp::Error> {
    let header = message.header.take().unwrap_or_default();
    let mut fields = Vec::with_capacity(16);
    fields.push(if header.durable { 0x41 } else { 0x42 });
    fields.extend([0x50, header.priority.0]);
    match header.ttl {
        Some(ttl) => {
            fields.push(0x70);
            fields.extend(ttl.to_be_bytes());
        }
        None => fields.push(0x40),
    }
    fields.push(if header.first_acquirer { 0x41 } else { 0x42 });
    fields.push(0x70);
    fields.extend(header.delivery_count.to_be_bytes());
    // Described list: descriptor 0x70 (header), list8 of five fields.
    let mut out = vec![0x00, 0x53, 0x70, 0xc0, (fields.len() + 1) as u8, 5];
    out.extend(fields);
    out.extend(serde_amqp::to_vec(&fe2o3_amqp_types::messaging::message::__private::Serializable(message))?);
    Ok(out.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_keeps_a_zero_delivery_count() {
        let message: AmqpMessage = Message::builder()
            .header(Header::default())
            .body(Body::Value(fe2o3_amqp_types::messaging::AmqpValue(Value::Bool(true))))
            .build();
        let bytes = encode(message).unwrap();
        assert_eq!(&bytes[..6], &[0x00, 0x53, 0x70, 0xc0, 11, 5]);
        let decoded: fe2o3_amqp_types::messaging::message::__private::Deserializable<AmqpMessage> =
            serde_amqp::from_slice(&bytes).unwrap();
        assert_eq!(decoded.0.header.unwrap().delivery_count, 0);
        assert_eq!(decoded.0.body, Body::Value(fe2o3_amqp_types::messaging::AmqpValue(Value::Bool(true))));
    }
}
