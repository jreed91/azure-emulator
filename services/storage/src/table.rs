//! The Table storage data plane, on `<account>.table.core.<suffix>`.
//!
//! Implemented: tables (list, create, get, delete, access policies) and
//! entities (insert, get, query with `$filter`, `$select` and `$top` and
//! continuation, update and merge with or without `If-Match`, delete), and
//! entity group transactions (`$batch`), in OData JSON at every metadata
//! level. Property types are kept as the client gave them (`@odata.type`)
//! and filters compare typed values. Requests are authorized with
//! `SharedKey` or `SharedKeyLite`, an account or table SAS signed with an
//! account key, or an Entra ID token. Tables are the same ones Resource
//! Manager lists under `tableServices/default/tables`.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::response::Response;
use base64::Engine as _;
use chrono::{DateTime, SecondsFormat, Utc};
use emulator_core::arm::{ArmState, Resource};
use emulator_core::auth::{self, AuthFailure};
use emulator_core::http_util::{parse_query, percent_decode, read_body, request_id};
use emulator_core::Engine;
use http::{header, HeaderMap, HeaderValue, Request, StatusCode};
use serde_json::{json, Map, Value};

use crate::service_properties::{self, Service};
use crate::sas;
use crate::shared_key;
use crate::state::StorageState;

const DEFAULT_VERSION: &str = "2019-02-02";
const TABLE_TYPE: &str = "Microsoft.Storage/storageAccounts/tableServices/tables";
const MAX_PAGE: usize = 1000;

/// One table: its name as created, its entities and access policies.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Table {
    pub name: String,
    /// Entities by (PartitionKey, RowKey); a snapshot keeps them as a list.
    #[serde(with = "entity_list")]
    entities: BTreeMap<(String, String), Entity>,
    acl: Option<String>,
}

/// Entities as `[[partition, row], entity]` pairs, since JSON keys are strings.
mod entity_list {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::Entity;

    pub(super) fn serialize<S: Serializer>(entities: &BTreeMap<(String, String), Entity>, serializer: S) -> Result<S::Ok, S::Error> {
        entities.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<BTreeMap<(String, String), Entity>, D::Error> {
        Ok(Vec::<((String, String), Entity)>::deserialize(deserializer)?.into_iter().collect())
    }
}

impl Table {
    pub fn new(name: &str) -> Self {
        Table {
            name: name.to_string(),
            entities: BTreeMap::new(),
            acl: None,
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Entity {
    /// Properties other than the keys and timestamp, with their
    /// `@odata.type` annotations as given.
    properties: Map<String, Value>,
    timestamp: DateTime<Utc>,
}

impl Entity {
    fn etag(&self) -> String {
        let time = self
            .timestamp
            .to_rfc3339_opts(SecondsFormat::Nanos, true)
            .replace(':', "%3A");
        format!("W/\"datetime'{time}'\"")
    }
}

#[derive(Debug)]
struct Failure {
    status: StatusCode,
    code: &'static str,
    message: String,
}

fn fail(status: StatusCode, code: &'static str, message: impl Into<String>) -> Failure {
    Failure {
        status,
        code,
        message: message.into(),
    }
}

fn bad(code: &'static str, message: impl Into<String>) -> Failure {
    fail(StatusCode::BAD_REQUEST, code, message)
}

fn table_not_found() -> Failure {
    fail(
        StatusCode::NOT_FOUND,
        "TableNotFound",
        "The table specified does not exist.",
    )
}

fn entity_not_found() -> Failure {
    fail(
        StatusCode::NOT_FOUND,
        "ResourceNotFound",
        "The specified resource does not exist.",
    )
}

fn error_body(failure: &Failure, id: &str) -> Value {
    json!({"odata.error": {"code": failure.code, "message": {
        "lang": "en-US",
        "value": format!("{}\nRequestId:{id}\nTime:{}", failure.message, emulator_core::http_util::iso_timestamp()),
    }}})
}

/// How much OData metadata a response carries.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Metadata {
    None,
    Minimal,
    Full,
}

fn metadata_level(headers: &HeaderMap) -> Metadata {
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if accept.contains("odata=nometadata") {
        Metadata::None
    } else if accept.contains("odata=fullmetadata") {
        Metadata::Full
    } else {
        Metadata::Minimal
    }
}

struct Reply {
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: Option<Value>,
    raw: Option<(String, String)>,
}

fn reply(status: StatusCode) -> Reply {
    Reply {
        status,
        headers: Vec::new(),
        body: None,
        raw: None,
    }
}

fn json_reply(status: StatusCode, body: Value) -> Reply {
    Reply {
        status,
        headers: Vec::new(),
        body: Some(body),
        raw: None,
    }
}

/// Handles one Table data-plane request.
pub async fn handle(engine: Arc<Engine>, account: String, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let id = request_id();
    let body = match read_body(body).await {
        Ok(body) => body.to_vec(),
        Err(response) => return response,
    };
    let result = serve(&engine, &account, &parts, &body);
    let reply = match result {
        Ok(reply) => reply,
        Err(failure) => {
            let mut reply = json_reply(failure.status, error_body(&failure, &id));
            reply
                .headers
                .push(("x-ms-error-code".into(), failure.code.into()));
            reply
        }
    };
    let version = parts
        .headers
        .get("x-ms-version")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(DEFAULT_VERSION)
        .to_string();
    let mut builder = Response::builder()
        .status(reply.status)
        .header(header::SERVER, "Azure-Emulator-Table/0.1")
        .header("x-ms-version", version)
        .header("x-ms-request-id", id)
        .header(
            "date",
            Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
        );
    if let Some(client) = parts.headers.get("x-ms-client-request-id") {
        builder = builder.header("x-ms-client-request-id", client.clone());
    }
    for (name, value) in reply.headers {
        if let Ok(value) = HeaderValue::from_str(&value) {
            builder = builder.header(name.as_str(), value);
        }
    }
    match (reply.body, reply.raw) {
        (_, Some((content_type, raw))) => builder
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(raw)),
        (Some(body), _) => builder
            .header(
                header::CONTENT_TYPE,
                "application/json;odata=minimalmetadata;streaming=true;charset=utf-8",
            )
            .body(Body::from(body.to_string())),
        _ => builder.body(Body::empty()),
    }
    .expect("valid response")
}

/// What a request does, for authorization.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    QueryTables,
    CreateTable,
    DeleteTable,
    GetAcl,
    SetAcl,
    ServiceProperties,
    SetServiceProperties,
    QueryEntities,
    InsertEntity,
    UpdateEntity,
    MergeEntity,
    DeleteEntity,
    Batch,
}

impl Op {
    fn sas(self) -> (char, &'static str) {
        match self {
            Op::QueryTables | Op::ServiceProperties => ('s', "l"),
            Op::SetServiceProperties => ('s', "w"),
            Op::CreateTable | Op::SetAcl => ('c', "c"),
            Op::DeleteTable => ('c', "d"),
            Op::GetAcl => ('c', "r"),
            Op::QueryEntities => ('o', "r"),
            Op::InsertEntity => ('o', "a"),
            Op::UpdateEntity | Op::MergeEntity => ('o', "u"),
            Op::DeleteEntity => ('o', "d"),
            Op::Batch => ('o', "audr"),
        }
    }

    fn permission(self) -> (bool, &'static str) {
        match self {
            Op::QueryTables | Op::GetAcl | Op::ServiceProperties => (
                false,
                "Microsoft.Storage/storageAccounts/tableServices/tables/read",
            ),
            Op::CreateTable | Op::SetAcl | Op::SetServiceProperties => (
                false,
                "Microsoft.Storage/storageAccounts/tableServices/tables/write",
            ),
            Op::DeleteTable => (
                false,
                "Microsoft.Storage/storageAccounts/tableServices/tables/delete",
            ),
            Op::QueryEntities => (
                true,
                "Microsoft.Storage/storageAccounts/tableServices/tables/entities/read",
            ),
            Op::InsertEntity | Op::Batch => (
                true,
                "Microsoft.Storage/storageAccounts/tableServices/tables/entities/add/action",
            ),
            Op::UpdateEntity | Op::MergeEntity => (
                true,
                "Microsoft.Storage/storageAccounts/tableServices/tables/entities/update/action",
            ),
            Op::DeleteEntity => (
                true,
                "Microsoft.Storage/storageAccounts/tableServices/tables/entities/delete",
            ),
        }
    }
}

/// A path's target: `Tables`, `Tables('name')`, `name`, `name()` or
/// `name(PartitionKey='a',RowKey='b')`.
enum Target {
    Root,
    Tables(Option<String>),
    Table {
        name: String,
        key: Option<(String, String)>,
    },
    Batch,
}

/// Reads a quoted OData string starting at `text[0] == '\''`.
fn quoted(text: &str) -> Option<(String, &str)> {
    let mut out = String::new();
    let mut chars = text.strip_prefix('\'')?.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '\'' {
            if chars.peek().map(|(_, n)| *n) == Some('\'') {
                out.push('\'');
                chars.next();
            } else {
                return Some((out, &text[i + 2..]));
            }
        } else {
            out.push(c);
        }
    }
    None
}

fn parse_target(path: &str) -> Option<Target> {
    let path = percent_decode(path.trim_start_matches('/'), false);
    if path.is_empty() {
        return Some(Target::Root);
    }
    if path == "$batch" {
        return Some(Target::Batch);
    }
    let (name, args) = match path.find('(') {
        Some(i) => (&path[..i], Some(path[i + 1..].strip_suffix(')')?)),
        None => (path.as_str(), None),
    };
    if name == "Tables" {
        return Some(Target::Tables(match args {
            Some(args) if !args.is_empty() => Some(quoted(args)?.0),
            _ => None,
        }));
    }
    let key = match args {
        None | Some("") => None,
        Some(args) => {
            let rest = args.strip_prefix("PartitionKey=")?;
            let (pk, rest) = quoted(rest)?;
            let rest = rest.strip_prefix(",RowKey=")?;
            let (rk, rest) = quoted(rest)?;
            if !rest.is_empty() {
                return None;
            }
            Some((pk, rk))
        }
    };
    Some(Target::Table {
        name: name.to_string(),
        key,
    })
}

fn method_name(parts: &http::request::Parts) -> String {
    // Some clients tunnel MERGE through POST.
    parts
        .headers
        .get("x-http-method")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(parts.method.as_str())
        .to_ascii_uppercase()
}

fn route(method: &str, target: &Target, query: &BTreeMap<String, String>) -> Option<Op> {
    let comp = query.get("comp").map(String::as_str);
    let restype = query.get("restype").map(String::as_str);
    Some(match (target, method) {
        (Target::Root, "GET") if comp == Some("properties") && restype == Some("service") => {
            Op::ServiceProperties
        }
        (Target::Root, "PUT") if comp == Some("properties") && restype == Some("service") => {
            Op::SetServiceProperties
        }
        (Target::Tables(_), "GET") => Op::QueryTables,
        (Target::Tables(None), "POST") => Op::CreateTable,
        (Target::Tables(Some(_)), "DELETE") => Op::DeleteTable,
        (Target::Table { key: None, .. }, "GET" | "HEAD") if comp == Some("acl") => Op::GetAcl,
        (Target::Table { key: None, .. }, "PUT") if comp == Some("acl") => Op::SetAcl,
        (Target::Table { .. }, "GET") => Op::QueryEntities,
        (Target::Table { key: None, .. }, "POST") => Op::InsertEntity,
        (Target::Table { key: Some(_), .. }, "PUT") => Op::UpdateEntity,
        (Target::Table { key: Some(_), .. }, "MERGE" | "PATCH") => Op::MergeEntity,
        (Target::Table { key: Some(_), .. }, "DELETE") => Op::DeleteEntity,
        (Target::Batch, "POST") => Op::Batch,
        _ => return None,
    })
}

fn table_sas_string(account: &str, table: &str, fields: &sas::Fields<'_>) -> String {
    format!(
        "{}\n{}\n{}\n/table/{account}/{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        fields.get("sp"),
        fields.get("st"),
        fields.get("se"),
        table.to_ascii_lowercase(),
        fields.get("si"),
        fields.get("sip"),
        fields.get("spr"),
        fields.get("sv"),
        fields.get("spk"),
        fields.get("srk"),
        fields.get("epk"),
        fields.get("erk"),
    )
}

fn authorize(
    engine: &Engine,
    account: &str,
    parts: &http::request::Parts,
    query: &BTreeMap<String, String>,
    table: &str,
    op: Op,
) -> Result<(), Failure> {
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
    let authorization = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    for (scheme, lite) in [("SharedKey ", false), ("SharedKeyLite ", true)] {
        if let Some(credential) = authorization.strip_prefix(scheme) {
            let (signed_account, signature) = credential.split_once(':').ok_or_else(failed)?;
            let keys = keys.clone().ok_or_else(failed)?;
            if signed_account != account {
                return Err(failed());
            }
            let to_sign = if lite {
                shared_key::table_lite_string_to_sign(&parts.uri, &parts.headers, account)
            } else {
                shared_key::table_string_to_sign(&parts.method, &parts.uri, &parts.headers, account)
            };
            return keys
                .iter()
                .any(|key| shared_key::verify(&to_sign, key, signature))
                .then_some(())
                .ok_or_else(failed);
        }
    }
    if query.contains_key("sig") {
        let keys = keys.ok_or_else(failed)?;
        let fields = sas::Fields(query);
        if !fields.get("si").is_empty() {
            return Err(fail(
                StatusCode::NOT_IMPLEMENTED,
                "NotImplemented",
                "The emulator verifies shared access signatures signed with an account key only.",
            ));
        }
        let is_account = !fields.get("ss").is_empty() || !fields.get("srt").is_empty();
        let sas_table = if is_account {
            table.to_string()
        } else {
            fields.get("tn").to_string()
        };
        let to_sign = if is_account {
            sas::account_string_to_sign(account, &fields)
        } else {
            table_sas_string(account, &sas_table, &fields)
        };
        if fields.get("sv").is_empty()
            || !sas::signature_matches(&keys, &to_sign, fields.get("sig"))
        {
            return Err(failed());
        }
        let now = Utc::now();
        let expires = sas::parse_time(fields.get("se")).ok_or_else(failed)?;
        if now >= expires || sas::parse_time(fields.get("st")).is_some_and(|start| now < start) {
            return Err(failed());
        }
        let (level, permissions) = op.sas();
        let mismatch = |code: &'static str| {
            fail(
                StatusCode::FORBIDDEN,
                code,
                "This request is not authorized to perform this operation.",
            )
        };
        if is_account {
            if !fields.get("ss").contains('t') {
                return Err(mismatch("AuthorizationServiceMismatch"));
            }
            if !fields.get("srt").contains(level) {
                return Err(mismatch("AuthorizationResourceTypeMismatch"));
            }
        } else if level != 'o' || !sas_table.eq_ignore_ascii_case(table) {
            return Err(mismatch("AuthorizationResourceTypeMismatch"));
        }
        let granted = fields.get("sp");
        let ok = if op == Op::Batch {
            granted.chars().any(|p| permissions.contains(p))
        } else {
            permissions.chars().any(|p| granted.contains(p))
        };
        if !ok {
            return Err(mismatch("AuthorizationPermissionMismatch"));
        }
        return Ok(());
    }
    let config = engine.config();
    let audiences = [
        "https://storage.azure.com".to_string(),
        format!("https://{account}.table.core.windows.net"),
        config.table_endpoint(account),
    ];
    match auth::authorize(&parts.headers, &audiences) {
        Ok(claims) => check_roles(engine, account, &claims.oid, table, op),
        Err(AuthFailure::Missing) => Err(fail(
            StatusCode::UNAUTHORIZED,
            "NoAuthenticationInformation",
            "Server failed to authenticate the request.",
        )),
        Err(_) => Err(fail(
            StatusCode::UNAUTHORIZED,
            "InvalidAuthenticationInfo",
            "Server failed to authenticate the request.",
        )),
    }
}

fn check_roles(
    engine: &Engine,
    account: &str,
    oid: &str,
    table: &str,
    op: Op,
) -> Result<(), Failure> {
    use emulator_core::arm::{data_access, Permission};
    if !engine.config().enforce_rbac {
        return Ok(());
    }
    let (data, action) = op.permission();
    let needed = if data {
        vec![Permission::Data(action)]
    } else {
        vec![Permission::Action(action)]
    };
    let account_id = account_resource_id(engine, account).unwrap_or_default();
    let scope = if table.is_empty() {
        account_id
    } else {
        format!("{account_id}/tableServices/default/tables/{table}")
    };
    if data_access(engine, oid, &needed, &scope) {
        return Ok(());
    }
    Err(fail(
        StatusCode::FORBIDDEN,
        "AuthorizationPermissionMismatch",
        "This request is not authorized to perform this operation using this permission.",
    ))
}

fn account_resource_id(engine: &Engine, account: &str) -> Option<String> {
    let state = engine.state::<ArmState>();
    let state = state.lock();
    let id = state
        .resources()
        .find(|r| {
            r.resource_type
                .eq_ignore_ascii_case("Microsoft.Storage/storageAccounts")
                && r.name.eq_ignore_ascii_case(account)
        })
        .map(|r| r.id.clone());
    id
}

/// Table names: 3 to 63 letters and numbers, starting with a letter.
pub(crate) fn valid_table_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (3..=63).contains(&bytes.len())
        && bytes[0].is_ascii_alphabetic()
        && bytes.iter().all(u8::is_ascii_alphanumeric)
        && !name.eq_ignore_ascii_case("tables")
}

fn valid_key(key: &str) -> bool {
    key.len() <= 1024
        && !key.contains(['/', '\\', '#', '?'])
        && !key.chars().any(|c| c.is_control())
}

/// A typed property value, for comparisons.
#[derive(Debug, Clone, PartialEq)]
enum Typed {
    String(String),
    Int(i64),
    Double(f64),
    Bool(bool),
    DateTime(DateTime<Utc>),
    Guid(String),
    Binary(Vec<u8>),
}

fn typed(value: &Value, annotation: Option<&str>) -> Option<Typed> {
    Some(match (annotation, value) {
        (Some("Edm.Int64"), Value::String(s)) => Typed::Int(s.parse().ok()?),
        (Some("Edm.Int64"), Value::Number(n)) => Typed::Int(n.as_i64()?),
        (Some("Edm.Double"), Value::String(s)) => Typed::Double(s.parse().ok()?),
        (Some("Edm.DateTime"), Value::String(s)) => {
            Typed::DateTime(DateTime::parse_from_rfc3339(s).ok()?.with_timezone(&Utc))
        }
        (Some("Edm.Guid"), Value::String(s)) => Typed::Guid(s.to_ascii_lowercase()),
        (Some("Edm.Binary"), Value::String(s)) => {
            Typed::Binary(base64::engine::general_purpose::STANDARD.decode(s).ok()?)
        }
        (_, Value::String(s)) => Typed::String(s.clone()),
        (_, Value::Bool(b)) => Typed::Bool(*b),
        (_, Value::Number(n)) => match n.as_i64() {
            Some(i) if annotation != Some("Edm.Double") => Typed::Int(i),
            _ => Typed::Double(n.as_f64()?),
        },
        _ => return None,
    })
}

fn compare(a: &Typed, b: &Typed) -> Option<Ordering> {
    match (a, b) {
        (Typed::String(x), Typed::String(y)) => Some(x.cmp(y)),
        (Typed::Int(x), Typed::Int(y)) => Some(x.cmp(y)),
        (Typed::Int(x), Typed::Double(y)) => (*x as f64).partial_cmp(y),
        (Typed::Double(x), Typed::Int(y)) => x.partial_cmp(&(*y as f64)),
        (Typed::Double(x), Typed::Double(y)) => x.partial_cmp(y),
        (Typed::Bool(x), Typed::Bool(y)) => Some(x.cmp(y)),
        (Typed::DateTime(x), Typed::DateTime(y)) => Some(x.cmp(y)),
        (Typed::Guid(x), Typed::Guid(y)) => Some(x.cmp(y)),
        (Typed::Binary(x), Typed::Binary(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// An OData `$filter` expression.
#[derive(Debug)]
enum Filter {
    And(Box<Filter>, Box<Filter>),
    Or(Box<Filter>, Box<Filter>),
    Not(Box<Filter>),
    Compare(String, &'static str, Typed),
    /// A boolean property on its own, or a literal.
    Property(String),
    Literal(bool),
}

struct FilterParser<'a> {
    text: &'a str,
}

impl<'a> FilterParser<'a> {
    fn skip(&mut self) {
        self.text = self.text.trim_start();
    }

    fn keyword(&mut self, word: &str) -> bool {
        self.skip();
        let len = word.len();
        if self.text.len() >= len
            && self.text[..len].eq_ignore_ascii_case(word)
            && self.text[len..]
                .chars()
                .next()
                .is_none_or(|c| c.is_whitespace() || c == '(')
        {
            self.text = &self.text[len..];
            true
        } else {
            false
        }
    }

    fn or(&mut self) -> Result<Filter, String> {
        let mut left = self.and()?;
        while self.keyword("or") {
            left = Filter::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Filter, String> {
        let mut left = self.unary()?;
        while self.keyword("and") {
            left = Filter::And(Box::new(left), Box::new(self.unary()?));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Filter, String> {
        if self.keyword("not") {
            return Ok(Filter::Not(Box::new(self.unary()?)));
        }
        self.skip();
        if let Some(rest) = self.text.strip_prefix('(') {
            self.text = rest;
            let inner = self.or()?;
            self.skip();
            self.text = self
                .text
                .strip_prefix(')')
                .ok_or("A parenthesis is not closed.")?;
            return Ok(inner);
        }
        if self.keyword("true") {
            return Ok(Filter::Literal(true));
        }
        if self.keyword("false") {
            return Ok(Filter::Literal(false));
        }
        let end = self
            .text
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(self.text.len());
        if end == 0 {
            return Err(format!(
                "Unexpected '{}' in the filter.",
                self.text.chars().next().unwrap_or(' ')
            ));
        }
        let name = self.text[..end].to_string();
        self.text = &self.text[end..];
        self.skip();
        for op in ["eq", "ne", "gt", "ge", "lt", "le"] {
            if self.keyword(op) {
                let value = self.literal()?;
                let op: &'static str = match op {
                    "eq" => "eq",
                    "ne" => "ne",
                    "gt" => "gt",
                    "ge" => "ge",
                    "lt" => "lt",
                    _ => "le",
                };
                return Ok(Filter::Compare(name, op, value));
            }
        }
        Ok(Filter::Property(name))
    }

    fn literal(&mut self) -> Result<Typed, String> {
        self.skip();
        let prefixed = |p: &str, text: &'a str| -> Option<(String, &'a str)> {
            let rest = text
                .get(..p.len())
                .filter(|h| h.eq_ignore_ascii_case(p))
                .map(|_| &text[p.len()..])?;
            quoted(rest)
        };
        if self.text.starts_with('\'') {
            let (s, rest) = quoted(self.text).ok_or("A string literal is not closed.")?;
            self.text = rest;
            return Ok(Typed::String(s));
        }
        if let Some((s, rest)) = prefixed("datetime", self.text) {
            self.text = rest;
            return DateTime::parse_from_rfc3339(&s)
                .map(|d| Typed::DateTime(d.with_timezone(&Utc)))
                .map_err(|_| format!("'{s}' is not a datetime."));
        }
        if let Some((s, rest)) = prefixed("guid", self.text) {
            self.text = rest;
            return Ok(Typed::Guid(s.to_ascii_lowercase()));
        }
        for p in ["binary", "X"] {
            if let Some((s, rest)) = prefixed(p, self.text) {
                self.text = rest;
                let bytes = (0..s.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(s.get(i..i + 2).unwrap_or("x"), 16))
                    .collect::<Result<Vec<_>, _>>();
                return bytes
                    .map(Typed::Binary)
                    .map_err(|_| format!("'{s}' is not hexadecimal."));
            }
        }
        if self.keyword("true") {
            return Ok(Typed::Bool(true));
        }
        if self.keyword("false") {
            return Ok(Typed::Bool(false));
        }
        let end = self
            .text
            .find(|c: char| {
                !(c.is_ascii_digit()
                    || matches!(c, '-' | '+' | '.' | 'e' | 'E' | 'L' | 'l' | 'd' | 'D'))
            })
            .unwrap_or(self.text.len());
        let token = &self.text[..end];
        self.text = &self.text[end..];
        if let Some(n) = token.strip_suffix(['L', 'l']) {
            return n
                .parse()
                .map(Typed::Int)
                .map_err(|_| format!("'{token}' is not a number."));
        }
        let n = token.trim_end_matches(['d', 'D']);
        if n.contains(['.', 'e', 'E']) || token.ends_with(['d', 'D']) {
            return n
                .parse()
                .map(Typed::Double)
                .map_err(|_| format!("'{token}' is not a number."));
        }
        n.parse()
            .map(Typed::Int)
            .map_err(|_| format!("'{token}' is not a value the filter knows."))
    }
}

fn parse_filter(text: &str) -> Result<Filter, String> {
    let mut parser = FilterParser { text };
    let filter = parser.or()?;
    parser.skip();
    if parser.text.is_empty() {
        Ok(filter)
    } else {
        Err(format!(
            "Unexpected '{}' at the end of the filter.",
            parser.text
        ))
    }
}

/// Every property of an entity, keys and timestamp included, typed.
fn property(pk: &str, rk: &str, entity: &Entity, name: &str) -> Option<Typed> {
    match name {
        "PartitionKey" => Some(Typed::String(pk.to_string())),
        "RowKey" => Some(Typed::String(rk.to_string())),
        "Timestamp" => Some(Typed::DateTime(entity.timestamp)),
        "TableName" => entity
            .properties
            .get("TableName")
            .and_then(|v| typed(v, None)),
        _ => typed(
            entity.properties.get(name)?,
            entity
                .properties
                .get(&format!("{name}@odata.type"))
                .and_then(Value::as_str),
        ),
    }
}

/// Whether the filter holds; a property the entity lacks makes a
/// comparison false.
fn matches(filter: &Filter, pk: &str, rk: &str, entity: &Entity) -> bool {
    match filter {
        Filter::And(a, b) => matches(a, pk, rk, entity) && matches(b, pk, rk, entity),
        Filter::Or(a, b) => matches(a, pk, rk, entity) || matches(b, pk, rk, entity),
        Filter::Not(inner) => !matches(inner, pk, rk, entity),
        Filter::Literal(b) => *b,
        Filter::Property(name) => property(pk, rk, entity, name) == Some(Typed::Bool(true)),
        Filter::Compare(name, op, value) => {
            let Some(actual) = property(pk, rk, entity, name) else {
                return false;
            };
            let Some(ordering) = compare(&actual, value) else {
                return *op == "ne";
            };
            match *op {
                "eq" => ordering == Ordering::Equal,
                "ne" => ordering != Ordering::Equal,
                "gt" => ordering == Ordering::Greater,
                "ge" => ordering != Ordering::Less,
                "lt" => ordering == Ordering::Less,
                _ => ordering != Ordering::Greater,
            }
        }
    }
}

fn endpoint(engine: &Engine, account: &str) -> String {
    engine.config().table_endpoint(account)
}

/// An entity as a response carries it.
#[allow(clippy::too_many_arguments)]
fn render(
    engine: &Engine,
    account: &str,
    table: &str,
    pk: &str,
    rk: &str,
    entity: &Entity,
    level: Metadata,
    select: Option<&[String]>,
) -> Value {
    let mut out = Map::new();
    let base = endpoint(engine, account);
    if level != Metadata::None {
        out.insert(
            "odata.metadata".into(),
            json!(format!("{base}$metadata#{table}/@Element")),
        );
        out.insert("odata.etag".into(), json!(entity.etag()));
    }
    if level == Metadata::Full {
        let id = format!(
            "{base}{table}(PartitionKey='{}',RowKey='{}')",
            pk.replace('\'', "''"),
            rk.replace('\'', "''")
        );
        out.insert("odata.type".into(), json!(format!("{account}.{table}")));
        out.insert("odata.id".into(), json!(id));
        out.insert("odata.editLink".into(), json!(id.trim_start_matches(&base)));
    }
    let wanted = |name: &str| select.is_none_or(|names| names.iter().any(|n| n == name));
    if wanted("PartitionKey") {
        out.insert("PartitionKey".into(), json!(pk));
    }
    if wanted("RowKey") {
        out.insert("RowKey".into(), json!(rk));
    }
    if wanted("Timestamp") {
        if level != Metadata::None {
            out.insert("Timestamp@odata.type".into(), json!("Edm.DateTime"));
        }
        out.insert(
            "Timestamp".into(),
            json!(entity.timestamp.to_rfc3339_opts(SecondsFormat::Nanos, true)),
        );
    }
    for (name, value) in &entity.properties {
        if let Some(base_name) = name.strip_suffix("@odata.type") {
            if level != Metadata::None && wanted(base_name) {
                out.insert(name.clone(), value.clone());
            }
            continue;
        }
        if wanted(name) {
            out.insert(name.clone(), value.clone());
            if level == Metadata::Full
                && !entity
                    .properties
                    .contains_key(&format!("{name}@odata.type"))
            {
                let edm = match value {
                    Value::Bool(_) => Some("Edm.Boolean"),
                    Value::Number(n) if n.is_i64() => Some("Edm.Int32"),
                    Value::Number(_) => Some("Edm.Double"),
                    Value::String(_) => Some("Edm.String"),
                    _ => None,
                };
                if let Some(edm) = edm {
                    out.insert(format!("{name}@odata.type"), json!(edm));
                }
            }
        }
    }
    // A selected property the entity lacks is null.
    for name in select.into_iter().flatten() {
        if !out.contains_key(name) {
            out.insert(name.clone(), Value::Null);
        }
    }
    Value::Object(out)
}

/// An entity body: its PartitionKey and RowKey, if given, and the rest.
type EntityBody = (Option<String>, Option<String>, Map<String, Value>);

/// Splits a body into the entity's user properties, checking them.
fn entity_body(
    body: &[u8],
) -> Result<EntityBody, Failure> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| bad("InvalidInput", "The request body is not valid JSON."))?;
    let Value::Object(mut map) = value else {
        return Err(bad("InvalidInput", "An entity must be a JSON object."));
    };
    let pk = map
        .remove("PartitionKey")
        .and_then(|v| v.as_str().map(str::to_string));
    let rk = map
        .remove("RowKey")
        .and_then(|v| v.as_str().map(str::to_string));
    for key in [
        "Timestamp",
        "Timestamp@odata.type",
        "odata.etag",
        "odata.metadata",
        "odata.type",
        "odata.id",
        "odata.editLink",
        "PartitionKey@odata.type",
        "RowKey@odata.type",
    ] {
        map.remove(key);
    }
    let properties = map.keys().filter(|k| !k.ends_with("@odata.type")).count();
    if properties > 252 {
        return Err(bad(
            "TooManyProperties",
            "The entity contains more properties than allowed.",
        ));
    }
    for (name, value) in &map {
        if let Some(base) = name.strip_suffix("@odata.type") {
            let Some(target) = map.get(base) else {
                continue;
            };
            if typed(target, value.as_str()).is_none() {
                return Err(bad(
                    "InvalidInput",
                    format!(
                        "The value of '{base}' is not a valid {}.",
                        value.as_str().unwrap_or("type")
                    ),
                ));
            }
        }
    }
    Ok((pk, rk, map))
}

fn with_tables<T>(
    engine: &Engine,
    account: &str,
    f: impl FnOnce(&mut BTreeMap<String, Table>) -> Result<T, Failure>,
) -> Result<T, Failure> {
    let state = engine.state::<StorageState>();
    let mut state = state.lock();
    let account = state.accounts.get_mut(account).ok_or_else(|| {
        fail(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            "The specified resource does not exist.",
        )
    })?;
    f(&mut account.tables)
}

/// A monotonic timestamp: two writes never share one, so etags differ.
fn next_timestamp(previous: Option<DateTime<Utc>>) -> DateTime<Utc> {
    let now = Utc::now();
    match previous {
        Some(p) if p >= now => p + chrono::Duration::microseconds(1),
        _ => now,
    }
}

/// One entity write, shared by the single operations and batches.
enum Write {
    Insert(Map<String, Value>),
    Replace {
        body: Map<String, Value>,
        if_match: Option<String>,
    },
    Merge {
        body: Map<String, Value>,
        if_match: Option<String>,
    },
    Delete {
        if_match: String,
    },
}

fn apply(table: &mut Table, pk: &str, rk: &str, write: Write) -> Result<Option<Entity>, Failure> {
    let key = (pk.to_string(), rk.to_string());
    let existing = table.entities.get(&key).cloned();
    let check = |if_match: &Option<String>, existing: &Option<Entity>| -> Result<(), Failure> {
        match (if_match.as_deref(), existing) {
            (None, _) => Ok(()),
            (Some(_), None) => Err(entity_not_found()),
            (Some("*"), Some(_)) => Ok(()),
            (Some(tag), Some(e)) if tag == e.etag() => Ok(()),
            _ => Err(fail(
                StatusCode::PRECONDITION_FAILED,
                "UpdateConditionNotSatisfied",
                "The update condition specified in the request was not satisfied.",
            )),
        }
    };
    match write {
        Write::Insert(body) => {
            if existing.is_some() {
                return Err(fail(
                    StatusCode::CONFLICT,
                    "EntityAlreadyExists",
                    "The specified entity already exists.",
                ));
            }
            let entity = Entity {
                properties: body,
                timestamp: next_timestamp(None),
            };
            table.entities.insert(key, entity.clone());
            Ok(Some(entity))
        }
        Write::Replace { body, if_match } => {
            check(&if_match, &existing)?;
            let entity = Entity {
                properties: body,
                timestamp: next_timestamp(existing.map(|e| e.timestamp)),
            };
            table.entities.insert(key, entity.clone());
            Ok(Some(entity))
        }
        Write::Merge { body, if_match } => {
            check(&if_match, &existing)?;
            let mut properties = existing
                .as_ref()
                .map(|e| e.properties.clone())
                .unwrap_or_default();
            for (name, value) in body {
                // A new value replaces the old type annotation too.
                if !name.ends_with("@odata.type") {
                    properties.remove(&format!("{name}@odata.type"));
                }
                properties.insert(name, value);
            }
            let entity = Entity {
                properties,
                timestamp: next_timestamp(existing.map(|e| e.timestamp)),
            };
            table.entities.insert(key, entity.clone());
            Ok(Some(entity))
        }
        Write::Delete { if_match } => {
            check(&Some(if_match), &existing)?;
            table.entities.remove(&key);
            Ok(None)
        }
    }
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn wants_content(headers: &HeaderMap, default: bool) -> bool {
    match header_text(headers, "prefer") {
        Some(p) if p.contains("return-no-content") => false,
        Some(p) if p.contains("return-content") => true,
        _ => default,
    }
}

fn serve(
    engine: &Engine,
    account: &str,
    parts: &http::request::Parts,
    body: &[u8],
) -> Result<Reply, Failure> {
    let query: BTreeMap<String, String> = parse_query(parts.uri.query())
        .into_iter()
        .map(|(k, v)| {
            (
                if k.starts_with('$') {
                    k
                } else {
                    k.to_ascii_lowercase()
                },
                v,
            )
        })
        .collect();
    let target = parse_target(parts.uri.path()).ok_or_else(|| {
        bad(
            "InvalidUri",
            "The requested URI does not represent any resource on the server.",
        )
    })?;
    let method = method_name(parts);
    let op = route(&method, &target, &query).ok_or_else(|| {
        fail(
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
            format!(
                "{method} {} is not implemented by the emulator.",
                parts.uri.path()
            ),
        )
    })?;
    let table_name = match &target {
        Target::Tables(Some(name)) => name.clone(),
        Target::Table { name, .. } => name.clone(),
        _ => String::new(),
    };
    authorize(engine, account, parts, &query, &table_name, op)?;
    let level = metadata_level(&parts.headers);
    let base = endpoint(engine, account);
    match op {
        Op::ServiceProperties => {
            let document = service_properties::read(engine, account, Service::Table)
                .ok_or_else(|| fail(StatusCode::NOT_FOUND, "ResourceNotFound", "The specified resource does not exist."))?;
            Ok(Reply {
                status: StatusCode::OK,
                headers: vec![],
                body: None,
                raw: Some(("application/xml".into(), document)),
            })
        }
        Op::SetServiceProperties => match service_properties::write(engine, account, Service::Table, &String::from_utf8_lossy(body)) {
            None => Err(fail(StatusCode::NOT_FOUND, "ResourceNotFound", "The specified resource does not exist.")),
            Some(Err(())) => Err(bad("InvalidXmlDocument", "XML specified is not syntactically valid.")),
            Some(Ok(())) => Ok(reply(StatusCode::ACCEPTED)),
        },
        Op::QueryTables => {
            let filter = query.get("$filter").map(|f| parse_filter(f)).transpose().map_err(|e| bad("InvalidInput", e))?;
            let top = query.get("$top").and_then(|t| t.parse::<usize>().ok()).unwrap_or(MAX_PAGE).clamp(1, MAX_PAGE);
            let start = query.get("nexttablename").cloned().unwrap_or_default().to_ascii_lowercase();
            let single = match &target {
                Target::Tables(Some(name)) => Some(name.to_ascii_lowercase()),
                _ => None,
            };
            let (names, next) = with_tables(engine, account, |tables| {
                if let Some(name) = &single {
                    let table = tables.get(name).ok_or_else(table_not_found)?;
                    return Ok((vec![table.name.clone()], None));
                }
                let mut names: Vec<(String, String)> = tables
                    .iter()
                    .filter(|(key, _)| key.as_str() >= start.as_str())
                    .filter(|(_, t)| {
                        filter.as_ref().is_none_or(|f| {
                            let mut properties = Map::new();
                            properties.insert("TableName".into(), json!(t.name));
                            matches(f, "", "", &Entity { properties, timestamp: Utc::now() })
                        })
                    })
                    .map(|(key, t)| (key.clone(), t.name.clone()))
                    .collect();
                let next = (names.len() > top).then(|| names[top].0.clone());
                names.truncate(top);
                Ok((names.into_iter().map(|(_, n)| n).collect(), next))
            })?;
            let row = |name: &str| {
                let mut row = Map::new();
                if level == Metadata::Full {
                    row.insert("odata.type".into(), json!(format!("{account}.Tables")));
                    row.insert("odata.id".into(), json!(format!("{base}Tables('{name}')")));
                    row.insert("odata.editLink".into(), json!(format!("Tables('{name}')")));
                }
                row.insert("TableName".into(), json!(name));
                Value::Object(row)
            };
            let body = if single.is_some() {
                let mut one = row(&names[0]);
                if level != Metadata::None {
                    one["odata.metadata"] = json!(format!("{base}$metadata#Tables/@Element"));
                }
                one
            } else {
                let mut list = json!({"value": names.iter().map(|n| row(n)).collect::<Vec<_>>()});
                if level != Metadata::None {
                    list["odata.metadata"] = json!(format!("{base}$metadata#Tables"));
                }
                list
            };
            let mut r = json_reply(StatusCode::OK, body);
            if let Some(next) = next {
                r.headers.push(("x-ms-continuation-NextTableName".into(), next));
            }
            Ok(r)
        }
        Op::CreateTable => {
            let value: Value = serde_json::from_slice(body).map_err(|_| bad("InvalidInput", "The request body is not valid JSON."))?;
            let name = value.get("TableName").and_then(Value::as_str).ok_or_else(|| bad("InvalidInput", "The TableName is required."))?.to_string();
            if !valid_table_name(&name) {
                return Err(bad("InvalidResourceName", "The specifed resource name contains invalid characters."));
            }
            with_tables(engine, account, |tables| {
                if tables.contains_key(&name.to_ascii_lowercase()) {
                    return Err(fail(StatusCode::CONFLICT, "TableAlreadyExists", "The table specified already exists."));
                }
                tables.insert(name.to_ascii_lowercase(), Table::new(&name));
                Ok(())
            })?;
            arm_added(engine, account, &name);
            if wants_content(&parts.headers, true) {
                let mut body = json!({"TableName": name});
                if level != Metadata::None {
                    body["odata.metadata"] = json!(format!("{base}$metadata#Tables/@Element"));
                }
                Ok(json_reply(StatusCode::CREATED, body))
            } else {
                let mut r = reply(StatusCode::NO_CONTENT);
                r.headers.push(("preference-applied".into(), "return-no-content".into()));
                Ok(r)
            }
        }
        Op::DeleteTable => {
            with_tables(engine, account, |tables| tables.remove(&table_name.to_ascii_lowercase()).map(|_| ()).ok_or_else(table_not_found))?;
            arm_removed(engine, account, &table_name);
            Ok(reply(StatusCode::NO_CONTENT))
        }
        Op::GetAcl => with_tables(engine, account, |tables| {
            let table = tables.get(&table_name.to_ascii_lowercase()).ok_or_else(table_not_found)?;
            let xml = table.acl.clone().unwrap_or_else(|| "<?xml version=\"1.0\" encoding=\"utf-8\"?><SignedIdentifiers />".into());
            Ok(Reply { status: StatusCode::OK, headers: vec![], body: None, raw: Some(("application/xml".into(), xml)) })
        }),
        Op::SetAcl => with_tables(engine, account, |tables| {
            let table = tables.get_mut(&table_name.to_ascii_lowercase()).ok_or_else(table_not_found)?;
            let xml = String::from_utf8_lossy(body).trim().to_string();
            table.acl = (!xml.is_empty()).then_some(xml);
            Ok(reply(StatusCode::NO_CONTENT))
        }),
        Op::QueryEntities => {
            let Target::Table { key, .. } = &target else { unreachable!() };
            let select: Option<Vec<String>> = query.get("$select").map(|s| s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect());
            with_tables(engine, account, |tables| {
                let table = tables.get(&table_name.to_ascii_lowercase()).ok_or_else(table_not_found)?;
                if let Some((pk, rk)) = key {
                    let entity = table.entities.get(&(pk.clone(), rk.clone())).ok_or_else(entity_not_found)?;
                    let mut r = json_reply(StatusCode::OK, render(engine, account, &table.name, pk, rk, entity, level, select.as_deref()));
                    r.headers.push(("etag".into(), entity.etag()));
                    return Ok(r);
                }
                let filter = query.get("$filter").map(|f| parse_filter(f)).transpose().map_err(|e| bad("InvalidInput", e))?;
                let top = query.get("$top").and_then(|t| t.parse::<usize>().ok()).unwrap_or(MAX_PAGE).clamp(1, MAX_PAGE);
                let from = (
                    query.get("nextpartitionkey").cloned().unwrap_or_default(),
                    query.get("nextrowkey").cloned().unwrap_or_default(),
                );
                let mut matched: Vec<(&(String, String), &Entity)> = table
                    .entities
                    .range(from..)
                    .filter(|((pk, rk), e)| filter.as_ref().is_none_or(|f| matches(f, pk, rk, e)))
                    .take(top + 1)
                    .collect();
                let next = (matched.len() > top).then(|| matched[top].0.clone());
                matched.truncate(top);
                let values: Vec<Value> = matched
                    .into_iter()
                    .map(|((pk, rk), e)| {
                        let mut v = render(engine, account, &table.name, pk, rk, e, level, select.as_deref());
                        if let Value::Object(map) = &mut v {
                            map.remove("odata.metadata");
                        }
                        v
                    })
                    .collect();
                let mut body = json!({"value": values});
                if level != Metadata::None {
                    body["odata.metadata"] = json!(format!("{base}$metadata#{}", table.name));
                }
                let mut r = json_reply(StatusCode::OK, body);
                if let Some((pk, rk)) = next {
                    r.headers.push(("x-ms-continuation-NextPartitionKey".into(), pk));
                    r.headers.push(("x-ms-continuation-NextRowKey".into(), rk));
                }
                Ok(r)
            })
        }
        Op::InsertEntity | Op::UpdateEntity | Op::MergeEntity | Op::DeleteEntity => {
            let Target::Table { key, .. } = &target else { unreachable!() };
            let if_match = header_text(&parts.headers, "if-match").map(str::to_string);
            let (pk, rk, write) = match op {
                Op::InsertEntity => {
                    let (pk, rk, body) = entity_body(body)?;
                    let (pk, rk) = (pk.ok_or_else(|| bad("PropertiesNeedValue", "The values are not specified for all properties in the entity: PartitionKey."))?, rk.ok_or_else(|| bad("PropertiesNeedValue", "The values are not specified for all properties in the entity: RowKey."))?);
                    (pk, rk, Write::Insert(body))
                }
                Op::DeleteEntity => {
                    let (pk, rk) = key.clone().expect("routed with a key");
                    let if_match = if_match.ok_or_else(|| bad("MissingRequiredHeader", "An HTTP header that's mandatory for this request is not specified: If-Match."))?;
                    (pk, rk, Write::Delete { if_match })
                }
                _ => {
                    let (pk, rk) = key.clone().expect("routed with a key");
                    let (body_pk, body_rk, body) = entity_body(body)?;
                    if body_pk.is_some_and(|b| b != pk) || body_rk.is_some_and(|b| b != rk) {
                        return Err(bad("PropertiesNeedValue", "The keys in the body do not match the URL."));
                    }
                    if op == Op::UpdateEntity { (pk, rk, Write::Replace { body, if_match }) } else { (pk, rk, Write::Merge { body, if_match }) }
                }
            };
            if !valid_key(&pk) || !valid_key(&rk) {
                return Err(bad("OutOfRangeInput", "The 'PartitionKey' or 'RowKey' parameter exceeds the maximum allowed length or contains an invalid character."));
            }
            with_tables(engine, account, |tables| {
                let table = tables.get_mut(&table_name.to_ascii_lowercase()).ok_or_else(table_not_found)?;
                let written = apply(table, &pk, &rk, write)?;
                let Some(entity) = written else { return Ok(reply(StatusCode::NO_CONTENT)) };
                let etag = entity.etag();
                if op == Op::InsertEntity && wants_content(&parts.headers, true) {
                    let mut r = json_reply(StatusCode::CREATED, render(engine, account, &table.name, &pk, &rk, &entity, level, None));
                    r.headers.push(("etag".into(), etag));
                    return Ok(r);
                }
                let mut r = reply(StatusCode::NO_CONTENT);
                r.headers.push(("etag".into(), etag));
                if op == Op::InsertEntity {
                    r.headers.push(("preference-applied".into(), "return-no-content".into()));
                }
                Ok(r)
            })
        }
        Op::Batch => batch(engine, account, parts, body),
    }
}

/// One part of a multipart body, split at `boundary`.
fn multipart_parts<'a>(body: &'a str, boundary: &str) -> Vec<&'a str> {
    let delimiter = format!("--{boundary}");
    body.split(delimiter.as_str())
        .skip(1)
        .take_while(|part| !part.starts_with("--"))
        .map(|part| part.trim_start_matches("\r\n").trim_start_matches('\n'))
        .collect()
}

fn boundary_of(content_type: &str) -> Option<String> {
    content_type.split(';').find_map(|p| {
        p.trim()
            .strip_prefix("boundary=")
            .map(|b| b.trim_matches('"').to_string())
    })
}

/// Splits headers from a body at the first blank line.
fn split_head(text: &str) -> (&str, &str) {
    text.split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .unwrap_or((text, ""))
}

/// An entity group transaction: every operation applies, or none does.
fn batch(
    engine: &Engine,
    account: &str,
    parts: &http::request::Parts,
    body: &[u8],
) -> Result<Reply, Failure> {
    let text = String::from_utf8_lossy(body).into_owned();
    let boundary = header_text(&parts.headers, "content-type")
        .and_then(boundary_of)
        .ok_or_else(|| bad("InvalidInput", "A batch needs a multipart boundary."))?;
    let changesets = multipart_parts(&text, &boundary);
    let changeset = changesets
        .first()
        .ok_or_else(|| bad("InvalidInput", "The batch is empty."))?;
    let (changeset_head, changeset_body) = split_head(changeset);
    let changeset_boundary = changeset_head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .starts_with("content-type")
                .then(|| boundary_of(l))
                .flatten()
        })
        .ok_or_else(|| bad("InvalidInput", "The changeset needs a multipart boundary."))?;

    struct Request {
        method: String,
        table: String,
        key: Option<(String, String)>,
        headers: HeaderMap,
        body: String,
    }
    let mut requests = Vec::new();
    for part in multipart_parts(changeset_body, &changeset_boundary) {
        let (_, http) = split_head(part);
        let (head, body) = split_head(http);
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default();
        let mut words = request_line.split_whitespace();
        let method = words.next().unwrap_or_default().to_ascii_uppercase();
        let url = words.next().unwrap_or_default();
        let path = url
            .split_once("://")
            .map(|(_, rest)| rest.split_once('/').map_or("", |(_, p)| p))
            .unwrap_or(url);
        let path = path.split('?').next().unwrap_or_default();
        let Some(Target::Table { name, key }) = parse_target(path) else {
            return Err(bad(
                "InvalidInput",
                format!("'{url}' is not an entity operation."),
            ));
        };
        let mut headers = HeaderMap::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                if let (Ok(k), Ok(v)) = (
                    http::HeaderName::from_bytes(k.trim().as_bytes()),
                    HeaderValue::from_str(v.trim()),
                ) {
                    headers.insert(k, v);
                }
            }
        }
        requests.push(Request {
            method,
            table: name,
            key,
            headers,
            body: body.trim_end_matches(['\r', '\n']).to_string(),
        });
    }
    if requests.is_empty() || requests.len() > 100 {
        return Err(bad(
            "InvalidInput",
            "A batch takes between 1 and 100 operations.",
        ));
    }

    let outcome = with_tables(engine, account, |tables| {
        let table_name = requests[0].table.to_ascii_lowercase();
        if requests
            .iter()
            .any(|r| r.table.to_ascii_lowercase() != table_name)
        {
            return Ok(Err((
                0,
                bad(
                    "InvalidInput",
                    "All operations in a batch must be on one table.",
                ),
            )));
        }
        let table = tables.get_mut(&table_name).ok_or_else(table_not_found)?;
        let snapshot = table.entities.clone();
        let mut partition: Option<String> = None;
        let mut results: Vec<(StatusCode, Option<String>, Option<Value>)> = Vec::new();
        for (index, request) in requests.iter().enumerate() {
            let if_match = header_text(&request.headers, "if-match").map(str::to_string);
            let outcome = (|| {
                let (pk, rk, write, status) = match (request.method.as_str(), &request.key) {
                    ("POST", None) => {
                        let (pk, rk, body) = entity_body(request.body.as_bytes())?;
                        let pk = pk.ok_or_else(|| {
                            bad("PropertiesNeedValue", "PartitionKey is required.")
                        })?;
                        let rk =
                            rk.ok_or_else(|| bad("PropertiesNeedValue", "RowKey is required."))?;
                        let status = if wants_content(&request.headers, true) {
                            StatusCode::CREATED
                        } else {
                            StatusCode::NO_CONTENT
                        };
                        (pk, rk, Write::Insert(body), status)
                    }
                    ("PUT", Some((pk, rk))) => {
                        let (_, _, body) = entity_body(request.body.as_bytes())?;
                        (
                            pk.clone(),
                            rk.clone(),
                            Write::Replace { body, if_match },
                            StatusCode::NO_CONTENT,
                        )
                    }
                    ("MERGE" | "PATCH", Some((pk, rk))) => {
                        let (_, _, body) = entity_body(request.body.as_bytes())?;
                        (
                            pk.clone(),
                            rk.clone(),
                            Write::Merge { body, if_match },
                            StatusCode::NO_CONTENT,
                        )
                    }
                    ("DELETE", Some((pk, rk))) => {
                        let if_match = if_match
                            .ok_or_else(|| bad("MissingRequiredHeader", "If-Match is required."))?;
                        (
                            pk.clone(),
                            rk.clone(),
                            Write::Delete { if_match },
                            StatusCode::NO_CONTENT,
                        )
                    }
                    (method, _) => {
                        return Err(bad(
                            "InvalidInput",
                            format!("'{method}' is not a batch operation."),
                        ))
                    }
                };
                if partition.get_or_insert_with(|| pk.clone()) != &pk {
                    return Err(bad(
                        "CommandsInBatchActOnDifferentPartitions",
                        "All commands in a batch must operate on the same partition.",
                    ));
                }
                let written = apply(table, &pk, &rk, write)?;
                let body = (status == StatusCode::CREATED).then(|| {
                    written
                        .as_ref()
                        .map(|e| {
                            render(
                                engine,
                                account,
                                &table.name,
                                &pk,
                                &rk,
                                e,
                                Metadata::Minimal,
                                None,
                            )
                        })
                        .unwrap_or(Value::Null)
                });
                Ok((status, written.map(|e| e.etag()), body))
            })();
            match outcome {
                Ok(result) => results.push(result),
                Err(failure) => {
                    table.entities = snapshot;
                    return Ok(Err((index, failure)));
                }
            }
        }
        Ok(Ok(results))
    })?;

    let response_boundary = format!("batchresponse_{}", uuid::Uuid::new_v4());
    let changeset_response = format!("changesetresponse_{}", uuid::Uuid::new_v4());
    let mut out = format!("--{response_boundary}\r\nContent-Type: multipart/mixed; boundary={changeset_response}\r\n\r\n");
    let status_line = |status: StatusCode| {
        format!(
            "HTTP/1.1 {} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("")
        )
    };
    match outcome {
        Ok(results) => {
            for (status, etag, body) in results {
                out.push_str(&format!("--{changeset_response}\r\nContent-Type: application/http\r\nContent-Transfer-Encoding: binary\r\n\r\n{}\r\n", status_line(status)));
                out.push_str("X-Content-Type-Options: nosniff\r\nCache-Control: no-cache\r\nDataServiceVersion: 3.0;\r\n");
                if let Some(etag) = etag {
                    out.push_str(&format!("ETag: {etag}\r\n"));
                }
                match body {
                    Some(body) => out.push_str(&format!("Content-Type: application/json;odata=minimalmetadata;streaming=true;charset=utf-8\r\n\r\n{body}\r\n")),
                    None => out.push_str("\r\n"),
                }
            }
        }
        Err((index, failure)) => {
            let mut error = error_body(&failure, &request_id());
            error["odata.error"]["message"]["value"] =
                json!(format!("{index}:{}", failure.message));
            out.push_str(&format!(
                "--{changeset_response}\r\nContent-Type: application/http\r\nContent-Transfer-Encoding: binary\r\n\r\n{}\r\nX-Content-Type-Options: nosniff\r\nDataServiceVersion: 3.0;\r\nContent-Type: application/json;odata=minimalmetadata;streaming=true;charset=utf-8\r\n\r\n{error}\r\n",
                status_line(failure.status)
            ));
        }
    }
    out.push_str(&format!(
        "--{changeset_response}--\r\n--{response_boundary}--\r\n"
    ));
    Ok(Reply {
        status: StatusCode::ACCEPTED,
        headers: vec![],
        body: None,
        raw: Some((
            format!("multipart/mixed; boundary={response_boundary}"),
            out,
        )),
    })
}

/// A table created on the data plane shows up on Resource Manager.
fn arm_added(engine: &Engine, account: &str, table: &str) {
    let Some(account_id) = account_resource_id(engine, account) else {
        return;
    };
    let id = format!("{account_id}/tableServices/default/tables/{table}");
    let state = engine.state::<ArmState>();
    let mut state = state.lock();
    if state.resource(&id).is_none() {
        state.insert(Resource {
            id,
            name: table.to_string(),
            resource_type: TABLE_TYPE.into(),
            location: None,
            tags: None,
            kind: None,
            sku: None,
            identity: None,
            properties: json!({"tableName": table}),
        });
    }
}

fn arm_removed(engine: &Engine, account: &str, table: &str) {
    let Some(account_id) = account_resource_id(engine, account) else {
        return;
    };
    engine.state::<ArmState>().lock().remove(&format!(
        "{account_id}/tableServices/default/tables/{table}"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(props: Value) -> Entity {
        Entity {
            properties: props.as_object().unwrap().clone(),
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn targets() {
        assert!(matches!(
            parse_target("/Tables"),
            Some(Target::Tables(None))
        ));
        assert!(
            matches!(parse_target("/Tables('people')"), Some(Target::Tables(Some(n))) if n == "people")
        );
        assert!(matches!(
            parse_target("/people()"),
            Some(Target::Table { key: None, .. })
        ));
        assert!(matches!(
            parse_target("/people(PartitionKey='a%27%27b',RowKey='1')"),
            Some(Target::Table { key: Some((pk, rk)), .. }) if pk == "a'b" && rk == "1"
        ));
    }

    #[test]
    fn filters() {
        let e = entity(
            json!({"Age": 31, "Big": "9000000000", "Big@odata.type": "Edm.Int64", "Name": "Ann", "Active": true,
            "Joined": "2020-01-02T00:00:00Z", "Joined@odata.type": "Edm.DateTime", "Score": 2.5}),
        );
        let check = |f: &str| {
            matches(
                &parse_filter(f).unwrap_or_else(|e| panic!("{f}: {e}")),
                "p1",
                "r1",
                &e,
            )
        };
        assert!(check("PartitionKey eq 'p1' and RowKey ge 'r'"));
        assert!(check("Age gt 30 and Age lt 40"));
        assert!(check("Big gt 8000000000L"));
        assert!(check("Name eq 'Ann' or Name eq 'Bob'"));
        assert!(check("not (Name eq 'Bob')"));
        assert!(check("Active eq true"));
        assert!(check("Active"));
        assert!(check("Joined ge datetime'2020-01-01T00:00:00Z'"));
        assert!(check("Score lt 3.0"));
        assert!(!check("Missing eq 1"));
        assert!(!check("Name eq 'ann'"));
        assert!(parse_filter("Age gt").is_err());
        assert!(parse_filter("(Age gt 1").is_err());
    }

    #[test]
    fn etags_change_with_every_write() {
        let mut table = Table::new("t");
        let first = apply(&mut table, "p", "r", Write::Insert(Map::new()))
            .unwrap()
            .unwrap()
            .etag();
        let second = apply(
            &mut table,
            "p",
            "r",
            Write::Merge {
                body: Map::new(),
                if_match: Some(first.clone()),
            },
        )
        .unwrap()
        .unwrap()
        .etag();
        assert_ne!(first, second);
        assert!(apply(&mut table, "p", "r", Write::Delete { if_match: first }).is_err());
    }
}
