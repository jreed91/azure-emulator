//! The Queue storage data plane, on `<account>.queue.core.<suffix>`.
//!
//! Implemented: list, create, delete queues; queue metadata and access
//! policies; service properties; put, get (dequeue with a pop receipt and a
//! visibility timeout), peek, update, delete and clear messages, with
//! message time to live. Requests are authorized with Shared Key, an account
//! or queue service SAS signed with an account key, or an Entra ID token (and
//! its role assignments, with `--enforce-rbac`). Queues are the same ones
//! Resource Manager lists under `queueServices/default/queues`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::response::Response;
use chrono::{DateTime, Utc};
use emulator_core::arm::{ArmState, Resource};
use emulator_core::auth::{self, AuthFailure};
use emulator_core::http_util::{parse_query, percent_decode, read_body, request_id};
use emulator_core::Engine;
use http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode};
use serde_json::json;

use crate::sas;
use crate::shared_key;
use crate::state::StorageState;

const DEFAULT_VERSION: &str = "2021-08-06";
/// Messages live seven days unless they say otherwise.
const DEFAULT_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const MAX_VISIBILITY: u64 = 7 * 24 * 3600;
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const QUEUE_TYPE: &str = "Microsoft.Storage/storageAccounts/queueServices/queues";

/// One queue's metadata, access policies and messages.
#[derive(Default)]
pub(crate) struct Queue {
    pub metadata: BTreeMap<String, String>,
    /// The `SignedIdentifiers` XML last set, as sent.
    pub acl: Option<String>,
    pub messages: Vec<Message>,
}

pub(crate) struct Message {
    id: String,
    text: String,
    inserted: SystemTime,
    /// `None` for a message that never expires.
    expires: Option<SystemTime>,
    visible_at: SystemTime,
    dequeue_count: u32,
    pop_receipt: Option<String>,
}

impl Queue {
    /// Drops expired messages.
    fn expire(&mut self, now: SystemTime) {
        self.messages.retain(|m| m.expires.is_none_or(|at| now < at));
    }

    /// An approximate count, as the service reports it: every message.
    pub fn count(&mut self) -> usize {
        self.expire(SystemTime::now());
        self.messages.len()
    }
}

struct Failure {
    status: StatusCode,
    code: &'static str,
    message: String,
}

fn fail(status: StatusCode, code: &'static str, message: impl Into<String>) -> Failure {
    Failure { status, code, message: message.into() }
}

fn queue_not_found() -> Failure {
    fail(StatusCode::NOT_FOUND, "QueueNotFound", "The specified queue does not exist.")
}

fn bad_value(name: &str) -> Failure {
    fail(StatusCode::BAD_REQUEST, "OutOfRangeQueryParameterValue", format!("The value for the query parameter '{name}' is out of range."))
}

fn xml_escape(input: &str) -> String {
    input.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// The text inside `<tag>...</tag>`, unescaped, if the element is there.
fn element(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    Some(
        xml[start..end]
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

fn http_date(time: SystemTime) -> String {
    DateTime::<Utc>::from(time).format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

struct Reply {
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: Option<String>,
}

fn reply(status: StatusCode) -> Reply {
    Reply { status, headers: Vec::new(), body: None }
}

fn xml_reply(status: StatusCode, body: String) -> Reply {
    Reply { status, headers: Vec::new(), body: Some(format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>{body}")) }
}

/// Handles one Queue data-plane request.
pub async fn handle(engine: Arc<Engine>, account: String, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let id = request_id();
    let body = match read_body(body).await {
        Ok(body) => String::from_utf8_lossy(&body).into_owned(),
        Err(response) => return response,
    };
    let result = serve(&engine, &account, &parts, &body);
    let (status, mut headers, body) = match result {
        Ok(r) => (r.status, r.headers, r.body),
        Err(f) => (
            f.status,
            vec![("x-ms-error-code".to_string(), f.code.to_string())],
            (parts.method != Method::HEAD).then(|| {
                format!(
                    "<?xml version=\"1.0\" encoding=\"utf-8\"?><Error><Code>{}</Code><Message>{}\nRequestId:{id}\nTime:{}</Message></Error>",
                    f.code,
                    xml_escape(&f.message),
                    emulator_core::http_util::iso_timestamp()
                )
            }),
        ),
    };
    let version = parts.headers.get("x-ms-version").and_then(|v| v.to_str().ok()).unwrap_or(DEFAULT_VERSION).to_string();
    headers.push(("x-ms-version".into(), version));
    headers.push(("x-ms-request-id".into(), id));
    if let Some(client) = parts.headers.get("x-ms-client-request-id").and_then(|v| v.to_str().ok()) {
        headers.push(("x-ms-client-request-id".into(), client.to_string()));
    }
    headers.push(("date".into(), http_date(SystemTime::now())));
    let mut builder = Response::builder().status(status).header(header::SERVER, "Azure-Emulator-Queue/0.1");
    for (name, value) in headers {
        if let Ok(value) = HeaderValue::from_str(&value) {
            builder = builder.header(name.as_str(), value);
        }
    }
    match body {
        Some(body) => builder.header(header::CONTENT_TYPE, "application/xml").body(Body::from(body)),
        None => builder.body(Body::empty()),
    }
    .expect("valid response")
}

/// The operations, for authorization.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    ListQueues,
    ServiceProperties,
    SetServiceProperties,
    CreateQueue,
    DeleteQueue,
    GetMetadata,
    SetMetadata,
    GetAcl,
    SetAcl,
    PutMessage,
    GetMessages,
    PeekMessages,
    ClearMessages,
    DeleteMessage,
    UpdateMessage,
}

impl Op {
    /// The SAS resource level and permissions this operation needs.
    fn sas(self) -> (char, &'static str) {
        match self {
            Op::ListQueues => ('s', "l"),
            Op::ServiceProperties | Op::SetServiceProperties => ('s', "rw"),
            Op::CreateQueue | Op::SetMetadata | Op::SetAcl => ('c', "c"),
            Op::DeleteQueue => ('c', "d"),
            Op::GetMetadata | Op::GetAcl => ('c', "r"),
            Op::PutMessage => ('o', "a"),
            Op::GetMessages | Op::DeleteMessage | Op::ClearMessages => ('o', "p"),
            Op::PeekMessages => ('o', "r"),
            Op::UpdateMessage => ('o', "u"),
        }
    }

    /// What an Entra ID caller's roles must allow, with `--enforce-rbac`.
    fn permission(self) -> (bool, &'static str) {
        match self {
            Op::ListQueues | Op::GetMetadata | Op::GetAcl | Op::ServiceProperties => (false, "Microsoft.Storage/storageAccounts/queueServices/queues/read"),
            Op::CreateQueue | Op::SetMetadata | Op::SetAcl | Op::SetServiceProperties => (false, "Microsoft.Storage/storageAccounts/queueServices/queues/write"),
            Op::DeleteQueue => (false, "Microsoft.Storage/storageAccounts/queueServices/queues/delete"),
            Op::PeekMessages => (true, "Microsoft.Storage/storageAccounts/queueServices/queues/messages/read"),
            Op::PutMessage => (true, "Microsoft.Storage/storageAccounts/queueServices/queues/messages/add/action"),
            Op::UpdateMessage => (true, "Microsoft.Storage/storageAccounts/queueServices/queues/messages/write"),
            Op::GetMessages => (true, "Microsoft.Storage/storageAccounts/queueServices/queues/messages/process/action"),
            Op::DeleteMessage | Op::ClearMessages => (true, "Microsoft.Storage/storageAccounts/queueServices/queues/messages/delete"),
        }
    }
}

fn route(method: &Method, segments: &[String], query: &BTreeMap<String, String>) -> Option<Op> {
    let comp = query.get("comp").map(String::as_str);
    let restype = query.get("restype").map(String::as_str);
    let peek = query.get("peekonly").is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let m = method.as_str();
    Some(match (segments.len(), m, comp, restype) {
        (0, "GET", Some("list"), _) => Op::ListQueues,
        (0, "GET", Some("properties"), Some("service")) => Op::ServiceProperties,
        (0, "PUT", Some("properties"), Some("service")) => Op::SetServiceProperties,
        (1, "PUT", None, _) => Op::CreateQueue,
        (1, "DELETE", None, _) => Op::DeleteQueue,
        (1, "GET" | "HEAD", Some("metadata"), _) => Op::GetMetadata,
        (1, "PUT", Some("metadata"), _) => Op::SetMetadata,
        (1, "GET" | "HEAD", Some("acl"), _) => Op::GetAcl,
        (1, "PUT", Some("acl"), _) => Op::SetAcl,
        (2, "POST", None, _) if segments[1] == "messages" => Op::PutMessage,
        (2, "GET", None, _) if segments[1] == "messages" && peek => Op::PeekMessages,
        (2, "GET", None, _) if segments[1] == "messages" => Op::GetMessages,
        (2, "DELETE", None, _) if segments[1] == "messages" => Op::ClearMessages,
        (3, "DELETE", None, _) if segments[1] == "messages" => Op::DeleteMessage,
        (3, "PUT", None, _) if segments[1] == "messages" => Op::UpdateMessage,
        _ => return None,
    })
}

/// The string a queue service SAS signs.
fn queue_sas_string(account: &str, queue: &str, fields: &sas::Fields<'_>) -> String {
    format!(
        "{}\n{}\n{}\n/queue/{account}/{queue}\n{}\n{}\n{}\n{}",
        fields.get("sp"),
        fields.get("st"),
        fields.get("se"),
        fields.get("si"),
        fields.get("sip"),
        fields.get("spr"),
        fields.get("sv"),
    )
}

fn authorize(engine: &Engine, account: &str, parts: &http::request::Parts, query: &BTreeMap<String, String>, queue: &str, op: Op) -> Result<(), Failure> {
    let failed = || {
        fail(
            StatusCode::FORBIDDEN,
            "AuthenticationFailed",
            "Server failed to authenticate the request. Make sure the value of Authorization header is formed correctly including the signature.",
        )
    };
    let keys = {
        let state = engine.state::<StorageState>();
        let state = state.lock();
        state.accounts.get(account).map(|a| a.keys.clone())
    };
    let authorization = parts.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("");
    if let Some(credential) = authorization.strip_prefix("SharedKey ") {
        let (signed_account, signature) = credential.split_once(':').ok_or_else(failed)?;
        let keys = keys.ok_or_else(failed)?;
        if signed_account != account {
            return Err(failed());
        }
        let to_sign = shared_key::string_to_sign(&parts.method, &parts.uri, &parts.headers, account);
        return keys.iter().any(|key| shared_key::verify(&to_sign, key, signature)).then_some(()).ok_or_else(failed);
    }
    if query.contains_key("sig") {
        let keys = keys.ok_or_else(failed)?;
        let fields = sas::Fields(query);
        if !fields.get("si").is_empty() || !fields.get("skoid").is_empty() {
            return Err(fail(
                StatusCode::NOT_IMPLEMENTED,
                "NotImplemented",
                "The emulator verifies account and service shared access signatures signed with an account key only.",
            ));
        }
        let is_account = !fields.get("ss").is_empty() || !fields.get("srt").is_empty();
        let to_sign = if is_account { sas::account_string_to_sign(account, &fields) } else { queue_sas_string(account, queue, &fields) };
        if fields.get("sv").is_empty() || !sas::signature_matches(&keys, &to_sign, fields.get("sig")) {
            return Err(failed());
        }
        let now = Utc::now();
        let expires = sas::parse_time(fields.get("se")).ok_or_else(failed)?;
        if now >= expires || sas::parse_time(fields.get("st")).is_some_and(|start| now < start) {
            return Err(failed());
        }
        let (level, permissions) = op.sas();
        let mismatch = |code: &'static str| fail(StatusCode::FORBIDDEN, code, "This request is not authorized to perform this operation.");
        if is_account {
            if !fields.get("ss").contains('q') {
                return Err(mismatch("AuthorizationServiceMismatch"));
            }
            if !fields.get("srt").contains(level) {
                return Err(mismatch("AuthorizationResourceTypeMismatch"));
            }
        } else if level != 'o' || queue.is_empty() {
            // A queue SAS covers one queue's messages only.
            return Err(mismatch("AuthorizationResourceTypeMismatch"));
        }
        if !permissions.chars().any(|p| fields.get("sp").contains(p)) {
            return Err(mismatch("AuthorizationPermissionMismatch"));
        }
        return Ok(());
    }
    let config = engine.config();
    let audiences = [
        "https://storage.azure.com".to_string(),
        format!("https://{account}.queue.core.windows.net"),
        config.queue_endpoint(account),
    ];
    match auth::authorize(&parts.headers, &audiences) {
        Ok(claims) => check_roles(engine, account, &claims.oid, queue, op),
        Err(AuthFailure::Missing) => Err(fail(
            StatusCode::UNAUTHORIZED,
            "NoAuthenticationInformation",
            "Server failed to authenticate the request. Please refer to the information in the www-authenticate header.",
        )),
        Err(_) => Err(fail(
            StatusCode::UNAUTHORIZED,
            "InvalidAuthenticationInfo",
            "Server failed to authenticate the request. Please refer to the information in the www-authenticate header.",
        )),
    }
}

fn check_roles(engine: &Engine, account: &str, oid: &str, queue: &str, op: Op) -> Result<(), Failure> {
    use emulator_core::arm::{data_access, Permission};
    if !engine.config().enforce_rbac {
        return Ok(());
    }
    let (data, action) = op.permission();
    let needed = if data { vec![Permission::Data(action)] } else { vec![Permission::Action(action)] };
    let account_id = account_resource_id(engine, account).unwrap_or_default();
    let scope = if queue.is_empty() { account_id } else { format!("{account_id}/queueServices/default/queues/{queue}") };
    if data_access(engine, oid, &needed, &scope) {
        return Ok(());
    }
    Err(fail(StatusCode::FORBIDDEN, "AuthorizationPermissionMismatch", "This request is not authorized to perform this operation using this permission."))
}

fn account_resource_id(engine: &Engine, account: &str) -> Option<String> {
    let state = engine.state::<ArmState>();
    let state = state.lock();
    let id = state
        .resources()
        .find(|r| r.resource_type.eq_ignore_ascii_case("Microsoft.Storage/storageAccounts") && r.name.eq_ignore_ascii_case(account))
        .map(|r| r.id.clone());
    id
}

/// Queue names: 3 to 63 lowercase letters, numbers and single hyphens.
fn valid_name(name: &str) -> bool {
    crate::blob::is_valid_container_name(name)
}

fn metadata_from(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str().strip_prefix("x-ms-meta-")?;
            Some((name.to_string(), value.to_str().ok()?.to_string()))
        })
        .collect()
}

fn number(query: &BTreeMap<String, String>, name: &str, default: i64, range: std::ops::RangeInclusive<i64>) -> Result<i64, Failure> {
    match query.get(name) {
        None => Ok(default),
        Some(text) => text.parse::<i64>().ok().filter(|n| range.contains(n)).ok_or_else(|| bad_value(name)),
    }
}

fn with_account<T>(engine: &Engine, account: &str, f: impl FnOnce(&mut BTreeMap<String, Queue>) -> Result<T, Failure>) -> Result<T, Failure> {
    let state = engine.state::<StorageState>();
    let mut state = state.lock();
    let account = state
        .accounts
        .get_mut(account)
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "ResourceNotFound", "The specified resource does not exist."))?;
    f(&mut account.queues)
}

fn with_queue<T>(engine: &Engine, account: &str, name: &str, f: impl FnOnce(&mut Queue) -> Result<T, Failure>) -> Result<T, Failure> {
    with_account(engine, account, |queues| {
        let queue = queues.get_mut(name).ok_or_else(queue_not_found)?;
        queue.expire(SystemTime::now());
        f(queue)
    })
}

fn serve(engine: &Engine, account: &str, parts: &http::request::Parts, body: &str) -> Result<Reply, Failure> {
    let query: BTreeMap<String, String> = parse_query(parts.uri.query()).into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect();
    let segments: Vec<String> = parts.uri.path().split('/').filter(|s| !s.is_empty()).map(|s| percent_decode(s, false)).collect();
    let op = route(&parts.method, &segments, &query)
        .ok_or_else(|| fail(StatusCode::NOT_IMPLEMENTED, "NotImplemented", format!("{} {} is not implemented by the emulator.", parts.method, parts.uri.path())))?;
    let queue = segments.first().cloned().unwrap_or_default();
    authorize(engine, account, parts, &query, &queue, op)?;
    let now = SystemTime::now();
    match op {
        Op::ListQueues => {
            let prefix = query.get("prefix").cloned().unwrap_or_default();
            let marker = query.get("marker").cloned().unwrap_or_default();
            let max = number(&query, "maxresults", 5000, 1..=5000)? as usize;
            let with_metadata = query.get("include").is_some_and(|i| i.contains("metadata"));
            let (items, next) = with_account(engine, account, |queues| {
                let mut names: Vec<(&String, &Queue)> =
                    queues.iter().filter(|(n, _)| n.starts_with(&prefix) && n.as_str() >= marker.as_str()).collect();
                let next = (names.len() > max).then(|| names[max].0.clone());
                names.truncate(max);
                let items: String = names
                    .iter()
                    .map(|(name, q)| {
                        let metadata = if with_metadata {
                            format!(
                                "<Metadata>{}</Metadata>",
                                q.metadata.iter().map(|(k, v)| format!("<{k}>{}</{k}>", xml_escape(v))).collect::<String>()
                            )
                        } else {
                            String::new()
                        };
                        format!("<Queue><Name>{}</Name>{metadata}</Queue>", xml_escape(name))
                    })
                    .collect();
                Ok((items, next))
            })?;
            Ok(xml_reply(
                StatusCode::OK,
                format!(
                    "<EnumerationResults ServiceEndpoint=\"{}\"><Prefix>{}</Prefix>{}<MaxResults>{max}</MaxResults><Queues>{items}</Queues><NextMarker>{}</NextMarker></EnumerationResults>",
                    xml_escape(&engine.config().queue_endpoint(account)),
                    xml_escape(&prefix),
                    if marker.is_empty() { String::new() } else { format!("<Marker>{}</Marker>", xml_escape(&marker)) },
                    next.map(|n| xml_escape(&n)).unwrap_or_default(),
                ),
            ))
        }
        Op::ServiceProperties => Ok(xml_reply(
            StatusCode::OK,
            "<StorageServiceProperties><Logging><Version>1.0</Version><Read>false</Read><Write>false</Write><Delete>false</Delete><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></Logging><HourMetrics><Version>1.0</Version><Enabled>false</Enabled><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></HourMetrics><MinuteMetrics><Version>1.0</Version><Enabled>false</Enabled><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></MinuteMetrics><Cors /></StorageServiceProperties>".into(),
        )),
        Op::SetServiceProperties => Ok(reply(StatusCode::ACCEPTED)),
        Op::CreateQueue => {
            if !valid_name(&queue) {
                return Err(fail(StatusCode::BAD_REQUEST, "InvalidResourceName", "The specifed resource name contains invalid characters."));
            }
            let metadata = metadata_from(&parts.headers);
            let created = with_account(engine, account, |queues| match queues.get(&queue) {
                Some(existing) if existing.metadata == metadata => Ok(false),
                Some(_) => Err(fail(StatusCode::CONFLICT, "QueueAlreadyExists", "The specified queue already exists.")),
                None => {
                    queues.insert(queue.clone(), Queue { metadata, ..Default::default() });
                    Ok(true)
                }
            })?;
            if created {
                arm_added(engine, account, &queue);
            }
            Ok(reply(if created { StatusCode::CREATED } else { StatusCode::NO_CONTENT }))
        }
        Op::DeleteQueue => {
            with_account(engine, account, |queues| queues.remove(&queue).map(|_| ()).ok_or_else(queue_not_found))?;
            arm_removed(engine, account, &queue);
            Ok(reply(StatusCode::NO_CONTENT))
        }
        Op::GetMetadata => with_queue(engine, account, &queue, |q| {
            let mut r = reply(StatusCode::OK);
            r.headers.push(("x-ms-approximate-messages-count".to_string(), q.messages.len().to_string()));
            for (k, v) in &q.metadata {
                r.headers.push((format!("x-ms-meta-{k}"), v.clone()));
            }
            Ok(r)
        }),
        Op::SetMetadata => with_queue(engine, account, &queue, |q| {
            q.metadata = metadata_from(&parts.headers);
            Ok(reply(StatusCode::NO_CONTENT))
        }),
        Op::GetAcl => with_queue(engine, account, &queue, |q| {
            let body = q.acl.clone().unwrap_or_else(|| "<SignedIdentifiers />".into());
            let body = body.trim_start_matches("<?xml version=\"1.0\" encoding=\"utf-8\"?>").to_string();
            Ok(xml_reply(StatusCode::OK, body))
        }),
        Op::SetAcl => with_queue(engine, account, &queue, |q| {
            q.acl = (!body.trim().is_empty()).then(|| body.trim().to_string());
            Ok(reply(StatusCode::NO_CONTENT))
        }),
        Op::PutMessage => {
            let text = element(body, "MessageText").ok_or_else(|| fail(StatusCode::BAD_REQUEST, "InvalidXmlDocument", "The message needs a MessageText."))?;
            if text.len() > MAX_MESSAGE_BYTES {
                return Err(fail(StatusCode::BAD_REQUEST, "RequestBodyTooLarge", "The message exceeds the 64 KiB limit."));
            }
            let visibility = number(&query, "visibilitytimeout", 0, 0..=MAX_VISIBILITY as i64)? as u64;
            let ttl = number(&query, "messagettl", DEFAULT_TTL.as_secs() as i64, -1..=i64::MAX)?;
            if ttl == 0 || (ttl > 0 && (ttl as u64) <= visibility) {
                return Err(bad_value("messagettl"));
            }
            let expires = (ttl > 0).then(|| now + Duration::from_secs(ttl as u64));
            let message = Message {
                id: uuid::Uuid::new_v4().to_string(),
                text,
                inserted: now,
                expires,
                visible_at: now + Duration::from_secs(visibility),
                dequeue_count: 0,
                pop_receipt: Some(pop_receipt()),
            };
            let entry = format!(
                "<QueueMessagesList><QueueMessage><MessageId>{}</MessageId><InsertionTime>{}</InsertionTime><ExpirationTime>{}</ExpirationTime><PopReceipt>{}</PopReceipt><TimeNextVisible>{}</TimeNextVisible></QueueMessage></QueueMessagesList>",
                message.id,
                http_date(message.inserted),
                expiry_text(message.expires),
                message.pop_receipt.clone().unwrap_or_default(),
                http_date(message.visible_at),
            );
            with_queue(engine, account, &queue, |q| {
                q.messages.push(message);
                Ok(())
            })?;
            Ok(xml_reply(StatusCode::CREATED, entry))
        }
        Op::GetMessages | Op::PeekMessages => {
            let count = number(&query, "numofmessages", 1, 1..=32)? as usize;
            let peek = op == Op::PeekMessages;
            let visibility = number(&query, "visibilitytimeout", 30, 1..=MAX_VISIBILITY as i64)? as u64;
            let entries = with_queue(engine, account, &queue, |q| {
                let mut out = String::new();
                for message in q.messages.iter_mut().filter(|m| m.visible_at <= now).take(count) {
                    if peek {
                        out.push_str(&format!(
                            "<QueueMessage><MessageId>{}</MessageId><InsertionTime>{}</InsertionTime><ExpirationTime>{}</ExpirationTime><DequeueCount>{}</DequeueCount><MessageText>{}</MessageText></QueueMessage>",
                            message.id,
                            http_date(message.inserted),
                            expiry_text(message.expires),
                            message.dequeue_count,
                            xml_escape(&message.text),
                        ));
                    } else {
                        message.dequeue_count += 1;
                        message.visible_at = now + Duration::from_secs(visibility);
                        let receipt = pop_receipt();
                        message.pop_receipt = Some(receipt.clone());
                        out.push_str(&format!(
                            "<QueueMessage><MessageId>{}</MessageId><InsertionTime>{}</InsertionTime><ExpirationTime>{}</ExpirationTime><PopReceipt>{receipt}</PopReceipt><TimeNextVisible>{}</TimeNextVisible><DequeueCount>{}</DequeueCount><MessageText>{}</MessageText></QueueMessage>",
                            message.id,
                            http_date(message.inserted),
                            expiry_text(message.expires),
                            http_date(message.visible_at),
                            message.dequeue_count,
                            xml_escape(&message.text),
                        ));
                    }
                }
                Ok(out)
            })?;
            Ok(xml_reply(StatusCode::OK, format!("<QueueMessagesList>{entries}</QueueMessagesList>")))
        }
        Op::ClearMessages => with_queue(engine, account, &queue, |q| {
            q.messages.clear();
            Ok(reply(StatusCode::NO_CONTENT))
        }),
        Op::DeleteMessage | Op::UpdateMessage => {
            let id = segments[2].clone();
            let receipt = query.get("popreceipt").cloned().ok_or_else(|| {
                fail(StatusCode::BAD_REQUEST, "MissingRequiredQueryParameter", "A query parameter that's mandatory for this request is not specified: popreceipt.")
            })?;
            let visibility = if op == Op::UpdateMessage { Some(number(&query, "visibilitytimeout", -1, 0..=MAX_VISIBILITY as i64)?) } else { None };
            if visibility == Some(-1) {
                return Err(fail(StatusCode::BAD_REQUEST, "MissingRequiredQueryParameter", "A query parameter that's mandatory for this request is not specified: visibilitytimeout."));
            }
            let new_text = if op == Op::UpdateMessage { element(body, "MessageText") } else { None };
            with_queue(engine, account, &queue, |q| {
                let index = q
                    .messages
                    .iter()
                    .position(|m| m.id == id)
                    .ok_or_else(|| fail(StatusCode::NOT_FOUND, "MessageNotFound", "The specified message does not exist."))?;
                if q.messages[index].pop_receipt.as_deref() != Some(receipt.as_str()) {
                    return Err(fail(StatusCode::BAD_REQUEST, "PopReceiptMismatch", "The specified pop receipt did not match the pop receipt for a dequeued message."));
                }
                if op == Op::DeleteMessage {
                    q.messages.remove(index);
                    return Ok(reply(StatusCode::NO_CONTENT));
                }
                let message = &mut q.messages[index];
                if let Some(text) = new_text {
                    message.text = text;
                }
                message.visible_at = now + Duration::from_secs(visibility.unwrap_or(0) as u64);
                let receipt = pop_receipt();
                message.pop_receipt = Some(receipt.clone());
                let mut r = reply(StatusCode::NO_CONTENT);
                r.headers.push(("x-ms-popreceipt".to_string(), receipt));
                r.headers.push(("x-ms-time-next-visible".to_string(), http_date(message.visible_at)));
                Ok(r)
            })
        }
    }
}

fn expiry_text(expires: Option<SystemTime>) -> String {
    match expires {
        Some(at) => http_date(at),
        // What the service reports for a message that never expires.
        None => "Fri, 31 Dec 9999 23:59:59 GMT".into(),
    }
}

fn pop_receipt() -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(uuid::Uuid::new_v4().as_bytes())
}

/// A queue created on the data plane shows up on Resource Manager.
fn arm_added(engine: &Engine, account: &str, queue: &str) {
    let Some(account_id) = account_resource_id(engine, account) else { return };
    let id = format!("{account_id}/queueServices/default/queues/{queue}");
    let state = engine.state::<ArmState>();
    let mut state = state.lock();
    if state.resource(&id).is_none() {
        state.insert(Resource {
            id,
            name: queue.to_string(),
            resource_type: QUEUE_TYPE.into(),
            location: None,
            tags: None,
            kind: None,
            sku: None,
            identity: None,
            properties: json!({"metadata": {}, "approximateMessageCount": 0}),
        });
    }
}

fn arm_removed(engine: &Engine, account: &str, queue: &str) {
    let Some(account_id) = account_resource_id(engine, account) else { return };
    engine.state::<ArmState>().lock().remove(&format!("{account_id}/queueServices/default/queues/{queue}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elements_unescape() {
        assert_eq!(element("<QueueMessage><MessageText>a &lt;b&gt; &amp; c</MessageText></QueueMessage>", "MessageText").as_deref(), Some("a <b> & c"));
        assert_eq!(element("<x/>", "MessageText"), None);
    }

    #[test]
    fn routes() {
        let q = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>();
        let seg = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(route(&Method::GET, &seg(&[]), &q(&[("comp", "list")])) == Some(Op::ListQueues));
        assert!(route(&Method::GET, &seg(&["q", "messages"]), &q(&[("peekonly", "true")])) == Some(Op::PeekMessages));
        assert!(route(&Method::PUT, &seg(&["q", "messages", "id"]), &q(&[])) == Some(Op::UpdateMessage));
        assert!(route(&Method::PATCH, &seg(&["q"]), &q(&[])).is_none());
    }
}
