//! The Service Bus data plane: an AMQP 1.0 broker serving every namespace.
//!
//! Clients connect with SASL ANONYMOUS and then authorize each entity by putting
//! a token on the `$cbs` node. The namespace and entity come from the link
//! addresses (`amqps://{namespace}.servicebus.<suffix>/{entity}`), so one
//! listener serves all namespaces. The SDKs connect to it in their development
//! emulator mode: `UseDevelopmentEmulator=true` in the connection string, which
//! `listKeys` returns.
//!
//! Implemented: queues and topic subscriptions with peek-lock (complete,
//! abandon, defer, dead-letter) and receive-and-delete; lock expiry and the
//! maximum delivery count; dead-letter subqueues; time to live; scheduled
//! messages; duplicate detection; auto-forwarding between queues and
//! subscriptions; batches; and the `$management` operations in [`management`].
//! Sessions, transactions and settling over the management link are not
//! implemented and are refused as such.

mod auth;
mod duration;
mod filters;
mod management;
pub(crate) mod store;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use emulator_core::Engine;
use fe2o3_amqp::acceptor::{
    ConnectionAcceptor, LinkAcceptor, LinkEndpoint, ListenerSessionHandle, SaslAnonymousMechanism,
    SessionAcceptor, SupportedReceiverSettleModes, SupportedSenderSettleModes,
};
use fe2o3_amqp::link::delivery::Sendable;
use fe2o3_amqp::{Receiver, Sender};
use fe2o3_amqp_types::definitions::{Error as AmqpError, ErrorCondition, Fields, SenderSettleMode};
use fe2o3_amqp_types::messaging::message::__private::Deserializable;
use fe2o3_amqp_types::messaging::{ApplicationProperties, Body, Message, Outcome, Properties};
use fe2o3_amqp_types::primitives::{OrderedMap, SimpleValue, Symbol, Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use auth::{Grant, Right};
use store::{AmqpMessage, Entity, Refusal, Settlement};
use fe2o3_amqp::link::delivery::DeliveryInfo;

pub use store::BrokerState;

/// The message format of a batch: data sections that each hold one message.
const BATCH_FORMAT: u32 = 0x8001_3700;

/// Serves AMQP connections from `listener` until the listener fails.
pub async fn serve(engine: Arc<Engine>, listener: TcpListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let engine = engine.clone();
        tokio::spawn(async move {
            let _ = connection(engine, stream).await;
        });
    }
}

/// What one connection shares across its sessions and links.
#[derive(Default)]
struct Context {
    grants: Mutex<Vec<Grant>>,
    /// Where replies to a request/response node go: the client's receiver link
    /// for that node address.
    replies: Mutex<HashMap<String, mpsc::UnboundedSender<AmqpMessage>>>,
}

impl Context {
    fn allows(&self, namespace: &str, path: &str, right: Right) -> bool {
        self.grants.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|g| g.allows(namespace, path, right))
    }

    /// The namespace and path an address names. A full address
    /// (`amqps://{namespace}.servicebus…/{path}`) names both; a bare path (how
    /// the SDKs address `$management` nodes) takes the namespace of a token put
    /// on this connection that covers it.
    fn entity_address(&self, address: &str) -> Option<(String, String)> {
        if address.contains("://") {
            return auth::split_uri(address).filter(|(_, path)| !path.is_empty());
        }
        let path = address.trim_matches('/').to_string();
        let grants = self.grants.lock().unwrap_or_else(|e| e.into_inner());
        let namespace = grants
            .iter()
            .find(|g| g.allows(g.namespace(), &path, Right::Listen) || g.allows(g.namespace(), &path, Right::Send))
            .or_else(|| grants.first())?
            .namespace()
            .to_string();
        (!path.is_empty()).then_some((namespace, path))
    }

    fn reply(&self, address: &str, message: AmqpMessage) {
        let replies = self.replies.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(route) = replies.get(&address.to_ascii_lowercase()) {
            let _ = route.send(message);
        }
    }
}

async fn connection(engine: Arc<Engine>, stream: tokio::net::TcpStream) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let acceptor = ConnectionAcceptor::builder()
        .container_id("azure-emulator")
        .sasl_acceptor(SaslAnonymousMechanism::new())
        .build();
    let mut connection = acceptor.accept(stream).await?;
    let context = Arc::new(Context::default());
    while let Some(incoming) = connection.next_incoming_session().await {
        let session = SessionAcceptor::new().accept_incoming_session(incoming, &mut connection).await?;
        tokio::spawn(session_loop(engine.clone(), context.clone(), session));
    }
    let _ = connection.close().await;
    Ok(())
}

fn error(condition: &str, description: impl Into<String>) -> AmqpError {
    AmqpError {
        condition: ErrorCondition::Custom(Symbol::from(condition)),
        description: Some(description.into()),
        info: None,
    }
}

fn refusal_error(refusal: &Refusal) -> AmqpError {
    error(refusal.condition, refusal.description.clone())
}

async fn session_loop(engine: Arc<Engine>, context: Arc<Context>, mut session: ListenerSessionHandle) {
    let acceptor = LinkAcceptor::builder()
        .supported_sender_settle_modes(SupportedSenderSettleModes::All)
        .supported_receiver_settle_modes(SupportedReceiverSettleModes::Both)
        .build();
    while let Some(attach) = session.next_incoming_attach().await {
        let settled = attach.snd_settle_mode == SenderSettleMode::Settled;
        let session_filter = attach
            .source
            .as_ref()
            .and_then(|s| s.filter.as_ref())
            .is_some_and(|f| f.keys().any(|k| k.0 == "com.microsoft:session-filter"));
        match acceptor.accept_incoming_attach(attach, &mut session).await {
            Ok(LinkEndpoint::Receiver(link)) => {
                tokio::spawn(incoming(engine.clone(), context.clone(), link));
            }
            Ok(LinkEndpoint::Sender(link)) => {
                tokio::spawn(outgoing(engine.clone(), context.clone(), link, settled, session_filter));
            }
            Err(_) => continue,
        }
    }
}

fn text(value: Option<&SimpleValue>) -> Option<String> {
    match value? {
        SimpleValue::String(s) => Some(s.clone()),
        SimpleValue::Symbol(s) => Some(s.0.clone()),
        _ => None,
    }
}

/// A link on which the client sends: to an entity, or requests to a node.
async fn incoming(engine: Arc<Engine>, context: Arc<Context>, mut link: Receiver) {
    let address = link.target().as_ref().and_then(|t| t.address.clone()).unwrap_or_default();
    let lower = address.to_ascii_lowercase();
    if lower == "$cbs" {
        return cbs_requests(engine, context, link, address).await;
    }
    if lower.ends_with("/$management") {
        return management_requests(engine, context, link, address).await;
    }
    let Some((namespace, path)) = context.entity_address(&address) else {
        let _ = link.close_with_error(error("amqp:invalid-field", format!("'{address}' is not an entity address."))).await;
        return;
    };
    let entity = match store::resolve(&engine, &namespace, &path) {
        Ok(entity) => entity,
        Err(refusal) => {
            let _ = link.close_with_error(refusal_error(&refusal)).await;
            return;
        }
    };
    if !context.allows(&namespace, &path, Right::Send) {
        let _ = link.close_with_error(error("amqp:unauthorized-access", "Unauthorized access. 'Send' claim(s) are required to perform this operation.")).await;
        return;
    }
    while let Ok(delivery) = link.recv::<Body<Value>>().await {
        let batch = delivery.message_format() == &Some(BATCH_FORMAT);
        let info = DeliveryInfo::from(&delivery).settle_on_disposition();
        let message = delivery.into_message();
        let messages = if batch { unbatch(message) } else { Ok(vec![message]) };
        // Re-read the entity: its settings may have changed since the attach.
        let result = messages
            .map_err(|description| store::refuse("amqp:decode-error", description))
            .and_then(|messages| {
                let entity = store::resolve(&engine, &entity.namespace, &entity.path)?;
                messages.into_iter().try_for_each(|m| store::send(&engine, &entity, m).map(|_| ()))
            });
        let _ = match result {
            Ok(()) => link.accept(info).await,
            Err(refusal) => link.reject(info, refusal_error(&refusal)).await,
        };
    }
}

/// The messages a batch carries, one per data section.
fn unbatch(batch: AmqpMessage) -> Result<Vec<AmqpMessage>, String> {
    match batch.body {
        Body::Data(sections) => sections
            .into_iter()
            .map(|data| {
                serde_amqp::from_slice::<Deserializable<Message<Body<Value>>>>(&data.0)
                    .map(|d| d.0)
                    .map_err(|e| format!("A message in the batch could not be decoded: {e}"))
            })
            .collect(),
        _ => Err("A batch must carry its messages in data sections.".into()),
    }
}

fn response(request: &AmqpMessage, status_field: &str, status: i32, description: &str) -> AmqpMessage {
    let mut application = OrderedMap::new();
    application.insert(status_field.to_string(), SimpleValue::Int(status));
    let description_field = if status_field == "status-code" { "status-description" } else { "statusDescription" };
    application.insert(description_field.to_string(), SimpleValue::String(description.to_string()));
    Message {
        header: None,
        delivery_annotations: None,
        message_annotations: None,
        properties: Some(Properties {
            correlation_id: request.properties.as_ref().and_then(|p| p.message_id.clone()),
            ..Default::default()
        }),
        application_properties: Some(ApplicationProperties(application)),
        body: Body::Value(fe2o3_amqp_types::messaging::AmqpValue(Value::Null)),
        footer: None,
    }
}

/// Requests to the `$cbs` node: tokens to put on the connection.
async fn cbs_requests(engine: Arc<Engine>, context: Arc<Context>, mut link: Receiver, address: String) {
    while let Ok(delivery) = link.recv::<Body<Value>>().await {
        let info = DeliveryInfo::from(&delivery).settle_on_disposition();
        let request = delivery.into_message();
        let _ = link.accept(info).await;
        let properties = request.application_properties.as_ref().map(|a| &a.0);
        let field = |name: &str| text(properties.and_then(|p| p.get(name)));
        let token = match &request.body {
            Body::Value(value) => match &value.0 {
                Value::String(token) => Some(token.clone()),
                _ => None,
            },
            _ => None,
        };
        let reply = match (field("operation").as_deref(), token, field("name"), field("type")) {
            (Some("put-token"), Some(token), Some(audience), Some(kind)) => match auth::verify(&engine, &kind, &token, &audience) {
                Ok(grant) => {
                    context.grants.lock().unwrap_or_else(|e| e.into_inner()).push(grant);
                    response(&request, "status-code", 202, "Accepted")
                }
                Err(reason) => response(&request, "status-code", 401, &reason),
            },
            _ => response(&request, "status-code", 400, "A put-token request needs a token, a name and a type."),
        };
        context.reply(&address, reply);
    }
}

/// Requests to an entity's `$management` node.
async fn management_requests(engine: Arc<Engine>, context: Arc<Context>, mut link: Receiver, address: String) {
    let entity_part = &address[..address.len() - "/$management".len()];
    while let Ok(delivery) = link.recv::<Body<Value>>().await {
        let info = DeliveryInfo::from(&delivery).settle_on_disposition();
        let request = delivery.into_message();
        let _ = link.accept(info).await;
        let reply = match context.entity_address(entity_part) {
            Some((namespace, path)) if context.allows(&namespace, &path, Right::Listen) => {
                match store::resolve(&engine, &namespace, &path) {
                    Ok(entity) => management::handle(&engine, &entity, &request),
                    Err(refusal) => response(&request, "statusCode", 404, &refusal.description),
                }
            }
            Some(_) => response(&request, "statusCode", 401, "Unauthorized access. 'Listen' claim(s) are required to perform this operation."),
            None => response(&request, "statusCode", 400, "The management node is not on an entity."),
        };
        context.reply(&address, reply);
    }
}

/// A link on which the client receives: replies from a node, or messages from
/// an entity.
async fn outgoing(engine: Arc<Engine>, context: Arc<Context>, link: Sender, remove: bool, session_filter: bool) {
    let address = link.source().as_ref().and_then(|s| s.address.clone()).unwrap_or_default();
    let lower = address.to_ascii_lowercase();
    if lower == "$cbs" || lower.ends_with("/$management") {
        return replies(context, link, lower).await;
    }
    let Some((namespace, path)) = context.entity_address(&address) else {
        let _ = link.close_with_error(error("amqp:invalid-field", format!("'{address}' is not an entity address."))).await;
        return;
    };
    let entity = match store::resolve(&engine, &namespace, &path) {
        Ok(entity) if entity.kind == store::Kind::Topic => {
            let _ = link.close_with_error(error("amqp:not-allowed", "Messages are received from a topic's subscriptions, not the topic.")).await;
            return;
        }
        Ok(entity) => entity,
        Err(refusal) => {
            let _ = link.close_with_error(refusal_error(&refusal)).await;
            return;
        }
    };
    if !context.allows(&namespace, &path, Right::Listen) {
        let _ = link.close_with_error(error("amqp:unauthorized-access", "Unauthorized access. 'Listen' claim(s) are required to perform this operation.")).await;
        return;
    }
    if session_filter || entity.requires_session() {
        let _ = link.close_with_error(error("amqp:not-implemented", "Session receivers are not supported by the emulator yet.")).await;
        return;
    }
    deliver(engine, link, entity, remove).await;
}

/// Sends a node's replies to the client's receiver link for it.
async fn replies(context: Arc<Context>, mut link: Sender, address: String) {
    let (route, mut queue) = mpsc::unbounded_channel();
    context.replies.lock().unwrap_or_else(|e| e.into_inner()).insert(address.clone(), route);
    loop {
        tokio::select! {
            message = queue.recv() => {
                let Some(message) = message else { break };
                if link.send_batchable(Sendable::builder().message(message).build()).await.is_err() {
                    break;
                }
            }
            _ = link.on_detach() => break,
        }
    }
    context.replies.lock().unwrap_or_else(|e| e.into_inner()).remove(&address);
}

/// How long one attempt to send waits for the client to grant credit before
/// the message is returned for other receivers.
const CREDIT_WAIT: Duration = Duration::from_millis(500);

/// Delivers an entity's messages to a receiver link until it detaches.
async fn deliver(engine: Arc<Engine>, mut link: Sender, entity: Entity, remove: bool) {
    let notify = engine.state::<BrokerState>().lock().notifier(&entity.key);
    loop {
        // Settings, including the status, are read fresh for each message.
        let entity = match store::resolve(&engine, &entity.namespace, &entity.path) {
            Ok(entity) => entity,
            Err(refusal) => {
                let _ = link.close_with_error(refusal_error(&refusal)).await;
                return;
            }
        };
        // Choose the message only once it can be sent: a message abandoned
        // meanwhile is then redelivered first, as on Service Bus.
        if tokio::time::timeout(Duration::from_millis(200), link.credit_available()).await.is_err() {
            if tokio::time::timeout(Duration::from_millis(1), link.on_detach()).await.is_ok() {
                return;
            }
            continue;
        }
        let available = entity.check_receive().ok().and_then(|()| store::take(&engine, &entity, remove));
        let Some(delivery) = available else {
            tokio::select! {
                _ = notify.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                _ = link.on_detach() => return,
            }
            continue;
        };
        // The lock token is the delivery tag: a 16-byte little-endian UUID.
        let sendable = Sendable::builder()
            .message(delivery.message)
            .settled(remove)
            .delivery_tag(delivery.lock_token.to_bytes_le().to_vec())
            .build();
        let sent = tokio::time::timeout(CREDIT_WAIT, link.send_batchable(sendable)).await;
        let outcome = match sent {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => {
                if !remove {
                    store::unreserve(&engine, &entity, delivery.sequence);
                }
                return;
            }
            Err(_) => {
                // No credit yet: give the message back and try again.
                if !remove {
                    store::unreserve(&engine, &entity, delivery.sequence);
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                    _ = link.on_detach() => return,
                }
                continue;
            }
        };
        if remove {
            continue;
        }
        store::lock(&engine, &entity, delivery.sequence, delivery.lock_token);
        let engine = engine.clone();
        let entity = entity.clone();
        tokio::spawn(async move {
            if let Ok(outcome) = outcome.await {
                store::settle(&engine, &entity, delivery.sequence, delivery.lock_token, settlement(outcome));
            }
        });
    }
}

fn fields_text(fields: Option<&Fields>, key: &str) -> Option<String> {
    match fields?.get(&Symbol::from(key))? {
        Value::String(s) => Some(s.clone()),
        Value::Symbol(s) => Some(s.0.clone()),
        _ => None,
    }
}

/// Properties a receiver asks to change when it settles.
fn properties_to_modify(fields: Option<&Fields>) -> Vec<(String, SimpleValue)> {
    fields
        .into_iter()
        .flat_map(|f| f.iter())
        .filter_map(|(key, value)| {
            let value = match value {
                Value::String(s) => SimpleValue::String(s.clone()),
                Value::Bool(b) => SimpleValue::Bool(*b),
                Value::Int(n) => SimpleValue::Int(*n),
                Value::Long(n) => SimpleValue::Long(*n),
                Value::Double(n) => SimpleValue::Double(*n),
                _ => return None,
            };
            Some((key.0.clone(), value))
        })
        .collect()
}

/// What a receiver's outcome means for the message.
fn settlement(outcome: Outcome) -> Settlement {
    match outcome {
        Outcome::Accepted(_) => Settlement::Complete,
        Outcome::Released(_) => Settlement::Abandon { failed: false, properties: Vec::new() },
        Outcome::Modified(modified) => {
            let properties = properties_to_modify(modified.message_annotations.as_ref());
            if modified.undeliverable_here == Some(true) {
                Settlement::Defer { properties }
            } else {
                Settlement::Abandon { failed: modified.delivery_failed.unwrap_or(false), properties }
            }
        }
        Outcome::Rejected(rejected) => {
            let info = rejected.error.as_ref().and_then(|e| e.info.as_deref());
            Settlement::DeadLetter {
                reason: fields_text(info, "DeadLetterReason").unwrap_or_default(),
                description: fields_text(info, "DeadLetterErrorDescription")
                    .or_else(|| rejected.error.as_ref().and_then(|e| e.description.clone()))
                    .unwrap_or_default(),
                properties: Vec::new(),
            }
        }
        #[allow(unreachable_patterns)]
        _ => Settlement::Abandon { failed: false, properties: Vec::new() },
    }
}
