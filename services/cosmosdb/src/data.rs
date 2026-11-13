//! The Cosmos DB for NoSQL data plane: the REST API the SDKs speak.
//!
//! Implemented: the account document (`GET /`); databases, containers and
//! items (create, read, replace, upsert, delete, patch, read feed); queries
//! and the query plans the SDKs ask for first; partition key ranges (one per
//! container); the change feed (incremental); transactional batches; the
//! `offers` the SDKs read throughput from. Authorization is a master or
//! read-only key signature, or an Entra ID token the emulator issued.

use std::sync::Arc;

use axum::body::Body;
use axum::response::Response;
use base64::Engine as _;
use emulator_core::arm::ArmState;
use emulator_core::http_util::{percent_decode, read_body};
use emulator_core::Engine;
use hmac::{Hmac, Mac};
use http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode};
use serde_json::{json, Map, Value};
use sha2::Sha256;

use crate::query;
use crate::store::{self, Account, Container, CosmosState, Database, Item, System};

const ACCOUNT_TYPE: &str = "Microsoft.DocumentDB/databaseAccounts";
const DEFAULT_PAGE: usize = 100;

/// A data-plane failure: status, Cosmos error code and message.
#[derive(Debug)]
pub(crate) struct Failure {
    status: StatusCode,
    code: &'static str,
    message: String,
}

fn fail(status: StatusCode, code: &'static str, message: impl Into<String>) -> Failure {
    Failure { status, code, message: message.into() }
}

fn bad_request(message: impl Into<String>) -> Failure {
    fail(StatusCode::BAD_REQUEST, "BadRequest", message)
}

fn not_found(what: &str) -> Failure {
    fail(StatusCode::NOT_FOUND, "NotFound", format!("Entity with the specified id does not exist in the system. ({what})"))
}

fn conflict(what: &str) -> Failure {
    fail(StatusCode::CONFLICT, "Conflict", format!("Entity with the specified id already exists in the system. ({what})"))
}

fn precondition() -> Failure {
    fail(StatusCode::PRECONDITION_FAILED, "PreconditionFailed", "Operation cannot be performed because one of the specified precondition is not met.")
}

/// Whether a request is meant for Cosmos DB: its authorization is a Cosmos
/// token (`type=master|aad|resource&ver=1.0&sig=...`).
pub fn is_cosmos_request(headers: &HeaderMap) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| percent_decode(v, false).to_ascii_lowercase())
        .is_some_and(|v| v.starts_with("type=") && v.contains("sig="))
}

/// The account a request is for: the host's first label when an account has
/// that name, else the only account there is.
fn account_for_host(engine: &Engine, host: &str) -> Option<String> {
    let first = host.split(['.', ':']).next().unwrap_or(host).to_ascii_lowercase();
    let state = engine.state::<CosmosState>();
    let state = state.lock();
    if state.accounts.contains_key(&first) {
        return Some(first);
    }
    let mut names = state.accounts.keys();
    match (names.next(), names.next()) {
        (Some(only), None) => Some(only.clone()),
        _ => None,
    }
}

struct Response2 {
    status: StatusCode,
    body: Option<Value>,
    headers: Vec<(&'static str, String)>,
}

fn ok(status: StatusCode, body: Value) -> Response2 {
    Response2 { status, body: Some(body), headers: vec![] }
}

impl Response2 {
    fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

fn finish(result: Result<Response2, Failure>, lsn: u64, request_charge: f64) -> Response {
    let (status, body, extra) = match result {
        Ok(r) => (r.status, r.body, r.headers),
        Err(f) => (
            f.status,
            Some(json!({"code": f.code, "message": format!("Message: {{\"Errors\":[{}]}}", json!(f.message))})),
            vec![("x-ms-substatus", "0".to_string())],
        ),
    };
    let mut builder = Response::builder()
        .status(status)
        .header("x-ms-activity-id", uuid::Uuid::new_v4().to_string())
        .header("x-ms-request-charge", format!("{request_charge:.2}"))
        .header("x-ms-session-token", format!("0:-1#{lsn}"))
        .header("lsn", lsn.to_string())
        .header("x-ms-schemaversion", "1.18")
        .header("x-ms-serviceversion", "version=2.14.0.0")
        .header("x-ms-gatewayversion", "version=2.14.0");
    for (name, value) in extra {
        if let Ok(value) = HeaderValue::from_str(&value) {
            builder = builder.header(name, value);
        }
    }
    match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("valid response"),
        None => builder.body(Body::empty()).expect("valid response"),
    }
}

/// Handles one data-plane request on the HTTPS port.
pub async fn handle(engine: Arc<Engine>, host: String, request: Request<Body>) -> Response {
    let Some(account) = account_for_host(&engine, &host) else {
        return finish(Err(fail(StatusCode::NOT_FOUND, "NotFound", format!("No Cosmos DB account answers at '{host}'."))), 0, 0.0);
    };
    let (parts, body) = request.into_parts();
    let body = match read_body(body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let path = parts.uri.path().to_string();
    let raw_segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let segments: Vec<String> = raw_segments.iter().map(|s| percent_decode(s, false)).collect();

    // What the signature covers: the resource type and the link to it.
    let (resource_type, link) = if segments.is_empty() {
        (String::new(), String::new())
    } else if segments.len() % 2 == 1 {
        (segments[segments.len() - 1].clone(), segments[..segments.len() - 1].join("/"))
    } else {
        (segments[segments.len() - 2].clone(), segments.join("/"))
    };
    let raw_link = if segments.len() % 2 == 1 { raw_segments[..raw_segments.len() - 1].join("/") } else { raw_segments.join("/") };
    let access = match authorize(&engine, &account, &parts.method, &parts.headers, &resource_type, &[link, raw_link]) {
        Ok(access) => access,
        Err(failure) => return finish(Err(failure), 0, 0.0),
    };
    let writes = !matches!(parts.method, Method::GET | Method::HEAD)
        && !(is_true(&parts.headers, "x-ms-documentdb-isquery")
            || is_true(&parts.headers, "x-ms-cosmos-is-query-plan-request")
            || parts.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("application/query+json")));
    if writes && access == Access::Read {
        return finish(Err(fail(StatusCode::FORBIDDEN, "Forbidden", "Request blocked: a read-only key cannot write.")), 0, 0.0);
    }
    let body: Option<Value> = if body.is_empty() {
        None
    } else {
        match serde_json::from_slice(&body) {
            Ok(value) => Some(value),
            Err(e) => return finish(Err(bad_request(format!("The request body is not valid JSON: {e}"))), 0, 0.0),
        }
    };
    let endpoint = format!("{}://{host}/", engine.config().scheme);
    let request = Req { method: parts.method, headers: parts.headers, segments, body, endpoint, account: account.clone() };
    let result = route(&engine, &request);
    let lsn = engine.state::<CosmosState>().lock().accounts.get(&account).map_or(0, |a| a.lsn);
    finish(result, lsn, 1.0)
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Access {
    Read,
    All,
}

type HmacSha256 = Hmac<Sha256>;

fn authorize(
    engine: &Engine,
    account: &str,
    method: &Method,
    headers: &HeaderMap,
    resource_type: &str,
    links: &[String],
) -> Result<Access, Failure> {
    let unauthorized = |message: &str| fail(StatusCode::UNAUTHORIZED, "Unauthorized", message.to_string());
    let raw = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).ok_or_else(|| unauthorized("Required Header authorization is missing."))?;
    let decoded = percent_decode(raw, false);
    let mut fields = std::collections::HashMap::new();
    for pair in decoded.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            fields.insert(k.to_ascii_lowercase(), v.to_string());
        }
    }
    let kind = fields.get("type").map(|t| t.to_ascii_lowercase()).unwrap_or_default();
    let signature = fields.get("sig").cloned().unwrap_or_default();
    match kind.as_str() {
        "master" => {
            let date = headers
                .get("x-ms-date")
                .or_else(|| headers.get(header::DATE))
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| unauthorized("The input authorization token can't serve the request. The x-ms-date header is missing."))?
                .to_ascii_lowercase();
            let keys = engine.state::<CosmosState>().lock().accounts.get(account).map(|a| a.keys.clone()).unwrap_or_default();
            let expected = base64::engine::general_purpose::STANDARD.decode(&signature).unwrap_or_default();
            for link in links {
                let text = format!("{}\n{}\n{}\n{}\n\n", method.as_str().to_ascii_lowercase(), resource_type.to_ascii_lowercase(), link, date);
                for (index, key) in keys.iter().enumerate() {
                    let Ok(key) = base64::engine::general_purpose::STANDARD.decode(key) else { continue };
                    let Ok(mut mac) = HmacSha256::new_from_slice(&key) else { continue };
                    mac.update(text.as_bytes());
                    if mac.verify_slice(&expected).is_ok() {
                        return Ok(if index < 2 { Access::All } else { Access::Read });
                    }
                }
            }
            Err(unauthorized(
                "The input authorization token can't serve the request. The wrong key is being used or the expected payload is not built as per the protocol.",
            ))
        }
        "aad" => {
            let audiences = vec![
                "https://cosmos.azure.com".to_string(),
                format!("https://{account}.documents.azure.com"),
                format!("https://{account}.documents.{}", engine.config().dns_suffix),
                format!("https://{account}.documents.{}:{}", engine.config().dns_suffix, engine.config().port),
                format!("https://localhost:{}", engine.config().port),
            ];
            emulator_core::auth::verify_token(&signature, &audiences)
                .map(|_| Access::All)
                .map_err(|_| unauthorized("The Entra ID token is not valid for this account."))
        }
        _ => Err(unauthorized("Only master key and Entra ID authorization are supported by the emulator.")),
    }
}

struct Req {
    method: Method,
    headers: HeaderMap,
    segments: Vec<String>,
    body: Option<Value>,
    endpoint: String,
    account: String,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

fn is_true(headers: &HeaderMap, name: &str) -> bool {
    headers.get(name).and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn route(engine: &Engine, req: &Req) -> Result<Response2, Failure> {
    let s: Vec<&str> = req.segments.iter().map(String::as_str).collect();
    let m = &req.method;
    match s.as_slice() {
        [] if m == Method::GET => Ok(ok(StatusCode::OK, account_document(&req.endpoint, &req.account))),
        ["dbs"] if m == Method::GET => list_databases(engine, req),
        ["dbs"] if m == Method::POST && is_query(req) => list_databases(engine, req),
        ["dbs"] if m == Method::POST => create_database(engine, req),
        ["dbs", db] if m == Method::GET => with_account(engine, req, |a| {
            let database = a.databases.get(*db).ok_or_else(|| not_found(db))?;
            Ok(ok(StatusCode::OK, database.json()).header("etag", database.system.etag.clone()))
        }),
        ["dbs", db] if m == Method::DELETE => {
            with_account(engine, req, |a| {
                a.databases.remove(*db).ok_or_else(|| not_found(db))?;
                a.bump();
                Ok(Response2 { status: StatusCode::NO_CONTENT, body: None, headers: vec![] })
            })
            .inspect(|_| crate::arm_sync::database_removed(engine, &req.account, db))
        }
        ["dbs", db, "colls"] if m == Method::GET || (m == Method::POST && is_query(req)) => list_containers(engine, req, db),
        ["dbs", db, "colls"] if m == Method::POST => create_container(engine, req, db),
        ["dbs", db, "colls", coll] if m == Method::GET => with_container(engine, req, db, coll, |db_rid, c| {
            Ok(ok(StatusCode::OK, c.json(db_rid)).header("etag", c.system.etag.clone()))
        }),
        ["dbs", db, "colls", coll] if m == Method::PUT => replace_container(engine, req, db, coll),
        ["dbs", db, "colls", coll] if m == Method::DELETE => {
            with_account(engine, req, |a| {
                let database = a.databases.get_mut(*db).ok_or_else(|| not_found(db))?;
                database.containers.remove(*coll).ok_or_else(|| not_found(coll))?;
                a.bump();
                Ok(Response2 { status: StatusCode::NO_CONTENT, body: None, headers: vec![] })
            })
            .inspect(|_| crate::arm_sync::container_removed(engine, &req.account, db, coll))
        }
        ["dbs", db, "colls", coll, "pkranges"] if m == Method::GET => partition_key_ranges(engine, req, db, coll),
        ["dbs", db, "colls", coll, "docs"] if m == Method::POST && is_true(&req.headers, "x-ms-cosmos-is-query-plan-request") => {
            query_plan(req)
        }
        ["dbs", db, "colls", coll, "docs"] if m == Method::POST && is_true(&req.headers, "x-ms-cosmos-is-batch-request") => {
            batch(engine, req, db, coll)
        }
        ["dbs", db, "colls", coll, "docs"] if m == Method::POST && is_query(req) => query_items(engine, req, db, coll),
        ["dbs", db, "colls", coll, "docs"] if m == Method::POST => create_item(engine, req, db, coll),
        ["dbs", db, "colls", coll, "docs"] if m == Method::GET => {
            if req.header("a-im").is_some_and(|v| v.eq_ignore_ascii_case("incremental feed")) {
                change_feed(engine, req, db, coll)
            } else {
                read_feed(engine, req, db, coll)
            }
        }
        ["dbs", db, "colls", coll, "docs", id] if m == Method::GET => read_item(engine, req, db, coll, id),
        ["dbs", db, "colls", coll, "docs", id] if m == Method::PUT => replace_item(engine, req, db, coll, id),
        ["dbs", db, "colls", coll, "docs", id] if m == Method::DELETE => delete_item(engine, req, db, coll, id),
        ["dbs", db, "colls", coll, "docs", id] if m == Method::PATCH => patch_item(engine, req, db, coll, id),
        ["offers"] if m == Method::GET || (m == Method::POST && is_query(req)) => offers(engine, req),
        ["offers", _] if m == Method::GET || m == Method::PUT => offer(engine, req),
        _ => Err(fail(
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
            format!("{} /{} is not implemented by the emulator.", req.method, req.segments.join("/")),
        )),
    }
}

fn is_query(req: &Req) -> bool {
    is_true(&req.headers, "x-ms-documentdb-isquery")
        || req.header("content-type").is_some_and(|v| v.starts_with("application/query+json"))
}

fn with_account<T>(engine: &Engine, req: &Req, f: impl FnOnce(&mut Account) -> Result<T, Failure>) -> Result<T, Failure> {
    let state = engine.state::<CosmosState>();
    let mut state = state.lock();
    let account = state.accounts.get_mut(&req.account).ok_or_else(|| not_found(&req.account))?;
    f(account)
}

fn with_container<T>(
    engine: &Engine,
    req: &Req,
    db: &str,
    coll: &str,
    f: impl FnOnce(&str, &mut Container) -> Result<T, Failure>,
) -> Result<T, Failure> {
    with_account(engine, req, |a| {
        let database = a.databases.get_mut(db).ok_or_else(|| not_found(db))?;
        let rid = database.system.rid.clone();
        let container = database.containers.get_mut(coll).ok_or_else(|| not_found(coll))?;
        container.expire();
        f(&rid, container)
    })
}

/// The account document the SDKs read first: where to read and write.
fn account_document(endpoint: &str, account: &str) -> Value {
    let location = json!({"name": "East US", "databaseAccountEndpoint": endpoint});
    json!({
        "_self": "",
        "id": account,
        "_rid": format!("{account}.documents.azure.com"),
        "media": "//media/",
        "addresses": "//addresses/",
        "_dbs": "//dbs/",
        "writableLocations": [location.clone()],
        "readableLocations": [location],
        "enableMultipleWriteLocations": false,
        "continuousBackupEnabled": false,
        "enableNRegionSynchronousCommit": false,
        "userReplicationPolicy": {"asyncReplication": false, "minReplicaSetSize": 1, "maxReplicasetSize": 4},
        "userConsistencyPolicy": {"defaultConsistencyLevel": "Session"},
        "systemReplicationPolicy": {"minReplicaSetSize": 1, "maxReplicasetSize": 4},
        "readPolicy": {"primaryReadCoefficient": 1, "secondaryReadCoefficient": 1},
        "queryEngineConfiguration": "{\"maxSqlQueryInputLength\":262144,\"maxJoinsPerSqlQuery\":5,\"maxLogicalAndPerSqlQuery\":500,\"maxLogicalOrPerSqlQuery\":500,\"maxUdfRefPerSqlQuery\":10,\"maxInExpressionItemsCount\":16000,\"queryMaxInMemorySortDocumentCount\":500,\"maxQueryRequestTimeoutFraction\":0.9,\"sqlAllowNonFiniteNumbers\":false,\"sqlAllowAggregateFunctions\":true,\"sqlAllowSubQuery\":true,\"sqlAllowScalarSubQuery\":true,\"allowNewKeywords\":true,\"sqlAllowLike\":true,\"sqlAllowGroupByClause\":true,\"maxSpatialQueryCells\":12,\"spatialMaxGeometryPointCount\":256,\"sqlDisableOptimizationFlags\":0,\"sqlAllowTop\":true,\"enableSpatialIndexing\":true}",
    })
}

/// Pages `items` by `x-ms-max-item-count` and `x-ms-continuation` (an offset).
fn page(req: &Req, items: Vec<Value>) -> Result<(Vec<Value>, Option<String>), Failure> {
    let size = req.header("x-ms-max-item-count").and_then(|v| v.parse::<i64>().ok()).filter(|n| *n > 0).map_or(DEFAULT_PAGE, |n| n as usize);
    let start = match req.header("x-ms-continuation").filter(|c| !c.is_empty()) {
        Some(token) => token.trim_matches('"').parse::<usize>().map_err(|_| bad_request("The continuation token is not valid."))?,
        None => 0,
    };
    let total = items.len();
    let page: Vec<Value> = items.into_iter().skip(start).take(size).collect();
    let next = (start + page.len() < total).then(|| (start + page.len()).to_string());
    Ok((page, next))
}

fn feed(req: &Req, rid: &str, key: &str, items: Vec<Value>) -> Result<Response2, Failure> {
    let (items, next) = page(req, items)?;
    let count = items.len();
    let mut response = ok(StatusCode::OK, json!({"_rid": rid, key: items, "_count": count})).header("x-ms-item-count", count.to_string());
    if let Some(next) = next {
        response = response.header("x-ms-continuation", next);
    }
    Ok(response)
}

fn body_object(req: &Req) -> Result<Map<String, Value>, Failure> {
    match &req.body {
        Some(Value::Object(map)) => Ok(map.clone()),
        _ => Err(bad_request("The request body must be a JSON object.")),
    }
}

fn throughput(req: &Req) -> Option<i64> {
    req.header("x-ms-offer-throughput").and_then(|v| v.parse().ok()).or_else(|| {
        req.header("x-ms-cosmos-offer-autopilot-settings")
            .and_then(|v| serde_json::from_str::<Value>(v).ok())
            .and_then(|v| v.get("maxThroughput").and_then(Value::as_i64))
    })
}

/// Filters a feed by a `query` body, for listing endpoints that accept one.
fn filter_listing(req: &Req, items: Vec<Value>) -> Result<Vec<Value>, Failure> {
    if !is_query(req) {
        return Ok(items);
    }
    let (text, params) = query_text(req)?;
    let parsed = query::parse(&text).map_err(bad_request)?;
    query::run(&parsed, items, &params).map_err(bad_request)
}

fn list_databases(engine: &Engine, req: &Req) -> Result<Response2, Failure> {
    let items = with_account(engine, req, |a| Ok(a.databases.values().map(Database::json).collect::<Vec<_>>()))?;
    feed(req, "", "Databases", filter_listing(req, items)?)
}

fn create_database(engine: &Engine, req: &Req) -> Result<Response2, Failure> {
    let body = body_object(req)?;
    let id = body.get("id").and_then(Value::as_str).ok_or_else(|| bad_request("The database needs an id."))?.to_string();
    store::validate_id(&id).map_err(bad_request)?;
    let json = with_account(engine, req, |a| {
        if a.databases.contains_key(&id) {
            return Err(conflict(&id));
        }
        let mut database = Database::new(&id);
        database.throughput = throughput(req);
        let json = database.json();
        a.databases.insert(id.clone(), database);
        a.bump();
        Ok(json)
    })?;
    crate::arm_sync::database_added(engine, &req.account, &id);
    let etag = json["_etag"].as_str().unwrap_or_default().to_string();
    Ok(ok(StatusCode::CREATED, json).header("etag", etag))
}

fn list_containers(engine: &Engine, req: &Req, db: &str) -> Result<Response2, Failure> {
    let (rid, items) = with_account(engine, req, |a| {
        let database = a.databases.get(db).ok_or_else(|| not_found(db))?;
        Ok((database.system.rid.clone(), database.containers.values().map(|c| c.json(&database.system.rid)).collect::<Vec<_>>()))
    })?;
    feed(req, &rid, "DocumentCollections", filter_listing(req, items)?)
}

fn create_container(engine: &Engine, req: &Req, db: &str) -> Result<Response2, Failure> {
    let definition = store::user_body(body_object(req)?);
    let mut container = Container::new(definition).map_err(bad_request)?;
    container.throughput = throughput(req);
    let id = container.id().to_string();
    let json = with_account(engine, req, |a| {
        let database = a.databases.get_mut(db).ok_or_else(|| not_found(db))?;
        if database.containers.contains_key(&id) {
            return Err(conflict(&id));
        }
        let json = container.json(&database.system.rid);
        database.containers.insert(id.clone(), container);
        a.bump();
        Ok(json)
    })?;
    crate::arm_sync::container_added(engine, &req.account, db, &id);
    let etag = json["_etag"].as_str().unwrap_or_default().to_string();
    Ok(ok(StatusCode::CREATED, json).header("etag", etag))
}

fn replace_container(engine: &Engine, req: &Req, db: &str, coll: &str) -> Result<Response2, Failure> {
    let definition = store::user_body(body_object(req)?);
    with_container(engine, req, db, coll, |db_rid, c| {
        if definition.get("id").and_then(Value::as_str) != Some(coll) {
            return Err(bad_request("The id of a container cannot change."));
        }
        if store::partition_key_paths(&definition) != c.paths() && definition.contains_key("partitionKey") {
            return Err(bad_request("The partition key of a container cannot change."));
        }
        check_if_match(req, &c.system.etag)?;
        let partition_key = c.definition.get("partitionKey").cloned();
        c.definition = definition;
        if let Some(pk) = partition_key {
            c.definition.insert("partitionKey".into(), pk);
        }
        c.system.touch();
        Ok(ok(StatusCode::OK, c.json(db_rid)).header("etag", c.system.etag.clone()))
    })
}

fn partition_key_ranges(engine: &Engine, req: &Req, db: &str, coll: &str) -> Result<Response2, Failure> {
    with_container(engine, req, db, coll, |db_rid, c| {
        // There is one range. A change-feed read of the ranges that already
        // has it is told nothing changed.
        if req.header("a-im").is_some() && req.header("if-none-match") == Some(c.ranges_etag.as_str()) {
            return Ok(Response2 { status: StatusCode::NOT_MODIFIED, body: None, headers: vec![("etag", c.ranges_etag.clone())] });
        }
        let range = json!({
            "id": "0",
            "_rid": format!("{}AgAAAAAAAAA=", c.system.rid),
            "_self": format!("dbs/{db_rid}/colls/{}/pkranges/0/", c.system.rid),
            "_etag": c.ranges_etag,
            "minInclusive": "",
            "maxExclusive": "FF",
            "ridPrefix": 0,
            "throughputFraction": 1,
            "status": "online",
            "parents": [],
            "_ts": c.system.ts,
            "_lsn": 1,
        });
        Ok(ok(StatusCode::OK, json!({"_rid": c.system.rid, "PartitionKeyRanges": [range], "_count": 1}))
            .header("x-ms-item-count", "1")
            .header("etag", c.ranges_etag.clone()))
    })
}

/// The partition key the request names, as a JSON array, if it names one.
fn header_key(req: &Req) -> Result<Option<Value>, Failure> {
    match req.header("x-ms-documentdb-partitionkey") {
        None => Ok(None),
        Some(text) => {
            let value: Value = serde_json::from_str(text).map_err(|_| bad_request("The partition key header is not valid JSON."))?;
            match value {
                Value::Array(items) if items.is_empty() => Ok(Some(json!([{}]))),
                Value::Array(_) => Ok(Some(value)),
                other => Ok(Some(json!([other]))),
            }
        }
    }
}

fn check_if_match(req: &Req, etag: &str) -> Result<(), Failure> {
    match req.header("if-match") {
        Some(expected) if expected != "*" && expected != etag => Err(precondition()),
        _ => Ok(()),
    }
}

fn item_response(status: StatusCode, item: &Item, db_rid: &str, c: &Container, req: &Req) -> Response2 {
    // Writes can ask for no content back.
    let minimal = req.header("prefer").is_some_and(|v| v.eq_ignore_ascii_case("return=minimal"));
    let body = (!minimal).then(|| item.json(db_rid, &c.system.rid));
    Response2 { status, body, headers: vec![("etag", item.system.etag.clone())] }
}

/// The key an item is stored under, checking the header against the body.
fn item_key(req: &Req, c: &Container, body: &Map<String, Value>) -> Result<String, Failure> {
    let actual = c.key_of(body);
    if let Some(given) = header_key(req)? {
        let system = c.definition.get("partitionKey").and_then(|p| p.get("systemKey")).and_then(Value::as_bool) == Some(true);
        if given != actual && !system {
            return Err(bad_request("PartitionKey extracted from document doesn't match the one specified in the header."));
        }
    }
    Ok(store::key_string(&actual))
}

fn check_unique(c: &Container, key: &str, id: &str, body: &Map<String, Value>) -> Result<(), Failure> {
    for paths in c.unique_keys() {
        let values: Vec<Option<&Value>> = paths.iter().map(|p| store::lookup_path(body, p)).collect();
        let clash = c.items.iter().any(|((k, other_id), item)| {
            k == key && other_id != id && paths.iter().map(|p| store::lookup_path(&item.body, p)).collect::<Vec<_>>() == values
        });
        if clash {
            return Err(fail(StatusCode::CONFLICT, "Conflict", "Unique index constraint violation."));
        }
    }
    Ok(())
}

fn write_item(account_lsn: u64, c: &mut Container, key: String, id: String, body: Map<String, Value>) {
    let entry = c.items.entry((key, id)).or_insert_with(|| Item { body: Map::new(), system: System::new(), lsn: 0 });
    entry.body = body;
    entry.system.touch();
    entry.lsn = account_lsn;
}

fn create_item(engine: &Engine, req: &Req, db: &str, coll: &str) -> Result<Response2, Failure> {
    let body = store::user_body(body_object(req)?);
    let id = body.get("id").and_then(Value::as_str).ok_or_else(|| bad_request("The input content is invalid because the required property, id, is missing."))?.to_string();
    store::validate_id(&id).map_err(bad_request)?;
    let upsert = is_true(&req.headers, "x-ms-documentdb-is-upsert");
    with_account(engine, req, |a| {
        let lsn = a.bump();
        let database = a.databases.get_mut(db).ok_or_else(|| not_found(db))?;
        let db_rid = database.system.rid.clone();
        let c = database.containers.get_mut(coll).ok_or_else(|| not_found(coll))?;
        c.expire();
        let key = item_key(req, c, &body)?;
        let exists = c.items.get(&(key.clone(), id.clone()));
        if let Some(existing) = exists {
            if !upsert {
                return Err(conflict(&id));
            }
            check_if_match(req, &existing.system.etag)?;
        }
        check_unique(c, &key, &id, &body)?;
        let status = if exists.is_some() { StatusCode::OK } else { StatusCode::CREATED };
        write_item(lsn, c, key.clone(), id.clone(), body);
        let item = c.items.get(&(key, id)).expect("just written");
        Ok(item_response(status, item, &db_rid, c, req))
    })
}

fn find_key(req: &Req, c: &Container) -> Result<String, Failure> {
    match header_key(req)? {
        Some(key) => Ok(store::key_string(&key)),
        None if c.paths() == ["/_partitionKey"] => Ok(store::key_string(&json!([{}]))),
        None => Err(bad_request("The partition key supplied in x-ms-partitionkey header has fewer components than defined in the collection.")),
    }
}

fn read_item(engine: &Engine, req: &Req, db: &str, coll: &str, id: &str) -> Result<Response2, Failure> {
    with_container(engine, req, db, coll, |db_rid, c| {
        let key = find_key(req, c)?;
        let item = c.items.get(&(key, id.to_string())).ok_or_else(|| not_found(id))?;
        if req.header("if-none-match") == Some(item.system.etag.as_str()) {
            return Ok(Response2 { status: StatusCode::NOT_MODIFIED, body: None, headers: vec![("etag", item.system.etag.clone())] });
        }
        Ok(ok(StatusCode::OK, item.json(db_rid, &c.system.rid)).header("etag", item.system.etag.clone()))
    })
}

fn replace_item(engine: &Engine, req: &Req, db: &str, coll: &str, id: &str) -> Result<Response2, Failure> {
    let body = store::user_body(body_object(req)?);
    if body.get("id").and_then(Value::as_str) != Some(id) {
        return Err(bad_request("The id in the body must match the item being replaced."));
    }
    with_account(engine, req, |a| {
        let lsn = a.bump();
        let database = a.databases.get_mut(db).ok_or_else(|| not_found(db))?;
        let db_rid = database.system.rid.clone();
        let c = database.containers.get_mut(coll).ok_or_else(|| not_found(coll))?;
        c.expire();
        let key = find_key(req, c)?;
        let existing = c.items.get(&(key.clone(), id.to_string())).ok_or_else(|| not_found(id))?;
        check_if_match(req, &existing.system.etag)?;
        if store::key_string(&c.key_of(&body)) != key {
            return Err(bad_request("PartitionKey extracted from document doesn't match the one specified in the header."));
        }
        check_unique(c, &key, id, &body)?;
        write_item(lsn, c, key.clone(), id.to_string(), body);
        let item = c.items.get(&(key, id.to_string())).expect("just written");
        Ok(item_response(StatusCode::OK, item, &db_rid, c, req))
    })
}

fn delete_item(engine: &Engine, req: &Req, db: &str, coll: &str, id: &str) -> Result<Response2, Failure> {
    with_account(engine, req, |a| {
        a.bump();
        let database = a.databases.get_mut(db).ok_or_else(|| not_found(db))?;
        let c = database.containers.get_mut(coll).ok_or_else(|| not_found(coll))?;
        c.expire();
        let key = find_key(req, c)?;
        let existing = c.items.get(&(key.clone(), id.to_string())).ok_or_else(|| not_found(id))?;
        check_if_match(req, &existing.system.etag)?;
        c.items.remove(&(key, id.to_string()));
        Ok(Response2 { status: StatusCode::NO_CONTENT, body: None, headers: vec![] })
    })
}

/// A JSON pointer's segments.
fn pointer(path: &str) -> Result<Vec<String>, Failure> {
    if !path.starts_with('/') {
        return Err(bad_request(format!("The patch path '{path}' must start with '/'.")));
    }
    Ok(path[1..].split('/').map(|s| s.replace("~1", "/").replace("~0", "~")).collect())
}

fn parent_of<'a>(root: &'a mut Value, segments: &[String]) -> Result<&'a mut Value, Failure> {
    let mut current = root;
    for segment in &segments[..segments.len() - 1] {
        current = match current {
            Value::Object(map) => map.get_mut(segment),
            Value::Array(items) => segment.parse::<usize>().ok().and_then(|i| items.get_mut(i)),
            _ => None,
        }
        .ok_or_else(|| bad_request(format!("The patch path segment '{segment}' does not exist.")))?;
    }
    Ok(current)
}

fn apply_patch(body: &mut Value, operation: &Map<String, Value>) -> Result<(), Failure> {
    let op = operation.get("op").and_then(Value::as_str).ok_or_else(|| bad_request("A patch operation needs an op."))?;
    let path = operation.get("path").and_then(Value::as_str).ok_or_else(|| bad_request("A patch operation needs a path."))?;
    let segments = pointer(path)?;
    let last = segments.last().cloned().unwrap_or_default();
    if segments.len() == 1 && (last == "id" || last.is_empty()) && op != "incr" && op != "set" && op != "replace" {
        return Err(bad_request("The id cannot be patched."));
    }
    let value = operation.get("value").cloned();
    match op {
        "move" => {
            let from = operation.get("from").and_then(Value::as_str).ok_or_else(|| bad_request("A move needs a from path."))?;
            let from_segments = pointer(from)?;
            let moved = {
                let parent = parent_of(body, &from_segments)?;
                let key = from_segments.last().cloned().unwrap_or_default();
                match parent {
                    Value::Object(map) => map.remove(&key),
                    _ => None,
                }
                .ok_or_else(|| bad_request(format!("The path '{from}' does not exist.")))?
            };
            let parent = parent_of(body, &segments)?;
            match parent {
                Value::Object(map) => {
                    map.insert(last, moved);
                    Ok(())
                }
                _ => Err(bad_request("A move's target must be an object property.")),
            }
        }
        _ => {
            let parent = parent_of(body, &segments)?;
            match parent {
                Value::Object(map) => match op {
                    "add" | "set" => {
                        map.insert(last, value.ok_or_else(|| bad_request("The operation needs a value."))?);
                        Ok(())
                    }
                    "replace" => match map.get_mut(&last) {
                        Some(slot) => {
                            *slot = value.ok_or_else(|| bad_request("The operation needs a value."))?;
                            Ok(())
                        }
                        None => Err(bad_request(format!("The path '{path}' does not exist."))),
                    },
                    "remove" => map.remove(&last).map(|_| ()).ok_or_else(|| bad_request(format!("The path '{path}' does not exist."))),
                    "incr" => {
                        let by = value.as_ref().and_then(Value::as_f64).ok_or_else(|| bad_request("incr needs a number."))?;
                        let slot = map.entry(last).or_insert(json!(0));
                        let current = slot.as_f64().ok_or_else(|| bad_request(format!("The value at '{path}' is not a number.")))?;
                        let both_ints = slot.is_i64() && value.as_ref().is_some_and(Value::is_i64);
                        *slot = if both_ints { json!((current + by) as i64) } else { json!(current + by) };
                        Ok(())
                    }
                    other => Err(bad_request(format!("'{other}' is not a patch operation."))),
                },
                Value::Array(items) => {
                    let index = if last == "-" { items.len() } else { last.parse::<usize>().map_err(|_| bad_request("An array index must be a number."))? };
                    match op {
                        "add" => {
                            if index > items.len() {
                                return Err(bad_request("The array index is out of range."));
                            }
                            items.insert(index, value.ok_or_else(|| bad_request("The operation needs a value."))?);
                            Ok(())
                        }
                        "set" | "replace" => {
                            let slot = items.get_mut(index).ok_or_else(|| bad_request("The array index is out of range."))?;
                            *slot = value.ok_or_else(|| bad_request("The operation needs a value."))?;
                            Ok(())
                        }
                        "remove" => {
                            if index >= items.len() {
                                return Err(bad_request("The array index is out of range."));
                            }
                            items.remove(index);
                            Ok(())
                        }
                        other => Err(bad_request(format!("'{other}' cannot be applied to an array element."))),
                    }
                }
                _ => Err(bad_request(format!("The path '{path}' does not lead into an object or array."))),
            }
        }
    }
}

fn patch_item(engine: &Engine, req: &Req, db: &str, coll: &str, id: &str) -> Result<Response2, Failure> {
    // The SDKs send either `{"operations": [...], "condition": ...}` or the
    // bare array of operations.
    let body = match &req.body {
        Some(Value::Array(operations)) => {
            let mut body = Map::new();
            body.insert("operations".into(), Value::Array(operations.clone()));
            body
        }
        _ => body_object(req)?,
    };
    let operations = body.get("operations").and_then(Value::as_array).cloned().ok_or_else(|| bad_request("A patch needs operations."))?;
    if operations.is_empty() || operations.len() > 10 {
        return Err(bad_request("A patch takes between 1 and 10 operations."));
    }
    let condition = body.get("condition").and_then(Value::as_str).map(str::to_string);
    with_account(engine, req, |a| {
        let lsn = a.bump();
        let database = a.databases.get_mut(db).ok_or_else(|| not_found(db))?;
        let db_rid = database.system.rid.clone();
        let c = database.containers.get_mut(coll).ok_or_else(|| not_found(coll))?;
        c.expire();
        let key = find_key(req, c)?;
        let existing = c.items.get(&(key.clone(), id.to_string())).ok_or_else(|| not_found(id))?;
        check_if_match(req, &existing.system.etag)?;
        if let Some(condition) = &condition {
            let text = if condition.trim_start().to_ascii_lowercase().starts_with("from") {
                format!("SELECT * {condition}")
            } else {
                condition.clone()
            };
            let parsed = query::parse(&text).map_err(bad_request)?;
            let matched = query::run(&parsed, vec![Value::Object(existing.body.clone())], &Map::new()).map_err(bad_request)?;
            if matched.is_empty() {
                return Err(precondition());
            }
        }
        let mut patched = Value::Object(existing.body.clone());
        for operation in &operations {
            apply_patch(&mut patched, operation.as_object().ok_or_else(|| bad_request("A patch operation must be an object."))?)?;
        }
        let Value::Object(patched) = patched else { unreachable!() };
        if store::key_string(&c.key_of(&patched)) != key {
            return Err(bad_request("A patch cannot change the partition key."));
        }
        check_unique(c, &key, id, &patched)?;
        write_item(lsn, c, key.clone(), id.to_string(), patched);
        let item = c.items.get(&(key, id.to_string())).expect("just written");
        Ok(item_response(StatusCode::OK, item, &db_rid, c, req))
    })
}

fn items_in_scope(req: &Req, c: &Container, db_rid: &str) -> Result<Vec<Value>, Failure> {
    let key = header_key(req)?.map(|k| store::key_string(&k));
    Ok(c
        .items
        .iter()
        .filter(|((k, _), _)| key.as_ref().is_none_or(|wanted| wanted == k))
        .map(|(_, item)| item.json(db_rid, &c.system.rid))
        .collect())
}

fn read_feed(engine: &Engine, req: &Req, db: &str, coll: &str) -> Result<Response2, Failure> {
    let (rid, items) = with_container(engine, req, db, coll, |db_rid, c| Ok((c.system.rid.clone(), items_in_scope(req, c, db_rid)?)))?;
    feed(req, &rid, "Documents", items)
}

/// The incremental change feed: the latest version of each item changed
/// after the continuation (a logical sequence number), oldest first.
fn change_feed(engine: &Engine, req: &Req, db: &str, coll: &str) -> Result<Response2, Failure> {
    let since: u64 = req.header("if-none-match").and_then(|v| v.trim_matches('"').parse().ok()).unwrap_or(0);
    let from_beginning = req.header("if-none-match").is_none();
    with_container(engine, req, db, coll, |db_rid, c| {
        let key = header_key(req)?.map(|k| store::key_string(&k));
        let mut changed: Vec<&Item> = c
            .items
            .iter()
            .filter(|((k, _), item)| key.as_ref().is_none_or(|wanted| wanted == k) && (from_beginning || item.lsn > since))
            .map(|(_, item)| item)
            .collect();
        changed.sort_by_key(|item| item.lsn);
        let size = req.header("x-ms-max-item-count").and_then(|v| v.parse::<i64>().ok()).filter(|n| *n > 0).map_or(DEFAULT_PAGE, |n| n as usize);
        changed.truncate(size);
        let last = changed.last().map_or(since, |item| item.lsn);
        let etag = format!("\"{last}\"");
        if changed.is_empty() {
            return Ok(Response2 { status: StatusCode::NOT_MODIFIED, body: None, headers: vec![("etag", etag)] });
        }
        let documents: Vec<Value> = changed
            .iter()
            .map(|item| {
                let mut json = item.json(db_rid, &c.system.rid);
                json["_lsn"] = json!(item.lsn);
                json
            })
            .collect();
        let count = documents.len();
        Ok(ok(StatusCode::OK, json!({"_rid": c.system.rid, "Documents": documents, "_count": count}))
            .header("etag", etag)
            .header("x-ms-item-count", count.to_string()))
    })
}

fn query_text(req: &Req) -> Result<(String, Map<String, Value>), Failure> {
    let body = body_object(req)?;
    let text = body.get("query").and_then(Value::as_str).ok_or_else(|| bad_request("The query body needs a query."))?.to_string();
    let mut params = Map::new();
    for parameter in body.get("parameters").and_then(Value::as_array).into_iter().flatten() {
        if let Some(name) = parameter.get("name").and_then(Value::as_str) {
            params.insert(name.to_string(), parameter.get("value").cloned().unwrap_or(Value::Null));
        }
    }
    Ok((text, params))
}

fn query_items(engine: &Engine, req: &Req, db: &str, coll: &str) -> Result<Response2, Failure> {
    let (text, params) = query_text(req)?;
    let parsed = query::parse(&text).map_err(bad_request)?;
    let (rid, items) = with_container(engine, req, db, coll, |db_rid, c| Ok((c.system.rid.clone(), items_in_scope(req, c, db_rid)?)))?;
    let results = query::run(&parsed, items, &params).map_err(bad_request)?;
    feed(req, &rid, "Documents", results)
}

/// The plan the SDKs fetch before a cross-partition query. Every container
/// has one partition key range and the query runs whole on the emulator, so
/// the plan asks the SDK for nothing but to pass the results through.
fn query_plan(req: &Req) -> Result<Response2, Failure> {
    let (text, _) = query_text(req)?;
    let parsed = query::parse(&text).map_err(bad_request)?;
    let plan = query::plan(&parsed);
    Ok(ok(
        StatusCode::OK,
        json!({
            "partitionedQueryExecutionInfoVersion": 2,
            "queryInfo": {
                "distinctType": "None",
                "top": null,
                "offset": null,
                "limit": null,
                "orderBy": [],
                "orderByExpressions": [],
                "groupByExpressions": [],
                "groupByAliases": [],
                "aggregates": [],
                "groupByAliasToAggregateType": {},
                "rewrittenQuery": "",
                "hasSelectValue": plan.has_select_value,
                "dCountInfo": null,
                "hasNonStreamingOrderBy": false
            },
            "queryRanges": [{"min": "", "max": "FF", "isMinInclusive": true, "isMaxInclusive": false}]
        }),
    ))
}

/// One batch operation's status and resulting item.
type BatchResult = (u16, Option<Map<String, Value>>);

/// The partition key a bulk operation carries: the JSON text of the key
/// (`"[\"a\"]"`, as the SDKs send it) or the key itself.
fn operation_key(operation: &Value) -> Option<Value> {
    let raw = operation.get("partitionKey")?;
    let value = match raw {
        Value::String(text) => serde_json::from_str(text).unwrap_or_else(|_| raw.clone()),
        other => other.clone(),
    };
    Some(match value {
        Value::Array(items) if items.is_empty() => json!([{}]),
        Value::Array(_) => value,
        other => json!([other]),
    })
}

/// One operation's outcome, with the partition key it ran in.
type Outcome = (String, Result<BatchResult, (u16, String)>);

/// Staged writes, by partition key and id; `None` is a delete.
type Staged = std::collections::BTreeMap<(String, String), Option<Map<String, Value>>>;

/// Applies one batch operation to `staged`, reading through it to the container.
fn batch_operation(c: &Container, staged: &mut Staged, key_text: &str, operation: &Value) -> Result<BatchResult, (u16, String)> {
    let op = operation.get("operationType").and_then(Value::as_str).unwrap_or_default();
    let resource = operation.get("resourceBody").and_then(Value::as_object).cloned().map(store::user_body);
    let body_id = resource.as_ref().and_then(|b| b.get("id")).and_then(Value::as_str).map(str::to_string);
    let id = operation
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(body_id)
        .ok_or((400, "The operation needs an id.".to_string()))?;
    let slot = (key_text.to_string(), id.clone());
    let existing = match staged.get(&slot) {
        Some(value) => value.clone(),
        None => c.items.get(&slot).map(|i| i.body.clone()),
    };
    if let (Some(expected), Some(_)) = (operation.get("ifMatch").and_then(Value::as_str), &existing) {
        let etag = c.items.get(&slot).map(|i| i.system.etag.clone()).unwrap_or_default();
        if expected != etag {
            return Err((412, "Precondition failed.".into()));
        }
    }
    let checked = |body: &Map<String, Value>| {
        if store::key_string(&c.key_of(body)) == key_text {
            Ok(())
        } else {
            Err((400, "The partition key does not match the operation's.".to_string()))
        }
    };
    match op {
        "Create" => {
            let body = resource.ok_or((400, "Create needs a body.".to_string()))?;
            checked(&body)?;
            if existing.is_some() {
                return Err((409, "Conflict.".into()));
            }
            staged.insert(slot, Some(body.clone()));
            Ok((201, Some(body)))
        }
        "Upsert" => {
            let body = resource.ok_or((400, "Upsert needs a body.".to_string()))?;
            checked(&body)?;
            let status = if existing.is_some() { 200 } else { 201 };
            staged.insert(slot, Some(body.clone()));
            Ok((status, Some(body)))
        }
        "Replace" => {
            let body = resource.ok_or((400, "Replace needs a body.".to_string()))?;
            checked(&body)?;
            existing.ok_or((404, "Not found.".to_string()))?;
            staged.insert(slot, Some(body.clone()));
            Ok((200, Some(body)))
        }
        "Read" => Ok((200, Some(existing.ok_or((404, "Not found.".to_string()))?))),
        "Delete" => {
            existing.ok_or((404, "Not found.".to_string()))?;
            staged.insert(slot, None);
            Ok((204, None))
        }
        "Patch" => {
            let mut body = Value::Object(existing.ok_or((404, "Not found.".to_string()))?);
            let ops = operation.get("resourceBody").and_then(|b| b.get("operations")).and_then(Value::as_array).cloned().unwrap_or_default();
            for patch in ops {
                let patch = patch.as_object().cloned().unwrap_or_default();
                apply_patch(&mut body, &patch).map_err(|f| (f.status.as_u16(), f.message))?;
            }
            let Value::Object(body) = body else { unreachable!() };
            checked(&body)?;
            staged.insert(slot, Some(body.clone()));
            Ok((200, Some(body)))
        }
        other => Err((400, format!("'{other}' is not a batch operation."))),
    }
}

fn commit(lsn: u64, c: &mut Container, staged: Staged) {
    for ((key, id), body) in staged {
        match body {
            Some(body) => {
                write_item(lsn, c, key, id, body);
            }
            None => {
                c.items.remove(&(key, id));
            }
        }
    }
}

/// A batch of item operations. A transactional batch (the default) is in one
/// partition, named by the request's partition key header, and every
/// operation applies or none does. A bulk request
/// (`x-ms-cosmos-batch-atomic: false`, as the SDKs' bulk APIs send) gives each
/// operation its own partition key, and each succeeds or fails on its own.
fn batch(engine: &Engine, req: &Req, db: &str, coll: &str) -> Result<Response2, Failure> {
    let operations = match &req.body {
        Some(Value::Array(ops)) => ops.clone(),
        _ => return Err(bad_request("A batch body must be an array of operations.")),
    };
    if operations.is_empty() || operations.len() > 100 {
        return Err(bad_request("A batch takes between 1 and 100 operations."));
    }
    let atomic = !req.header("x-ms-cosmos-batch-atomic").is_some_and(|v| v.eq_ignore_ascii_case("false"));
    let header = header_key(req)?;
    if atomic && header.is_none() {
        return Err(bad_request("A batch needs a partition key."));
    }
    with_account(engine, req, |a| {
        let lsn = a.bump();
        let database = a.databases.get_mut(db).ok_or_else(|| not_found(db))?;
        let db_rid = database.system.rid.clone();
        let c = database.containers.get_mut(coll).ok_or_else(|| not_found(coll))?;
        c.expire();
        let mut results: Vec<Outcome> = Vec::new();
        if atomic {
            let key_text = store::key_string(header.as_ref().expect("checked above"));
            // Work on a copy; keep it only if every operation succeeds.
            let mut staged = Staged::new();
            for (index, operation) in operations.iter().enumerate() {
                let outcome = batch_operation(c, &mut staged, &key_text, operation);
                if let Err((status, _)) = outcome {
                    // Every other operation answers 424 Failed Dependency.
                    let entries: Vec<Value> = (0..operations.len())
                        .map(|i| json!({"statusCode": if i == index { status } else { 424 }, "requestCharge": 0}))
                        .collect();
                    return Ok(ok(StatusCode::MULTI_STATUS, Value::Array(entries)));
                }
                results.push((key_text.clone(), outcome));
            }
            commit(lsn, c, staged);
        } else {
            for operation in &operations {
                let outcome = match operation_key(operation).or_else(|| header.clone()) {
                    None => Err((400, "The operation needs a partition key.".to_string())),
                    Some(key) => {
                        let key_text = store::key_string(&key);
                        let mut staged = Staged::new();
                        let outcome = batch_operation(c, &mut staged, &key_text, operation);
                        if outcome.is_ok() {
                            commit(lsn, c, staged);
                        }
                        results.push((key_text, outcome));
                        continue;
                    }
                };
                results.push((String::new(), outcome));
            }
        }
        let any_failed = results.iter().any(|(_, r)| r.is_err());
        let entries: Vec<Value> = results
            .into_iter()
            .map(|(key_text, outcome)| match outcome {
                Err((status, message)) => json!({"statusCode": status, "requestCharge": 0, "message": message}),
                Ok((status, body)) => {
                    let mut entry = json!({"statusCode": status, "requestCharge": 1});
                    if let Some(body) = body {
                        let id = body.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                        if let Some(item) = c.items.get(&(key_text, id)) {
                            entry["resourceBody"] = item.json(&db_rid, &c.system.rid);
                            entry["eTag"] = json!(item.system.etag);
                        } else {
                            entry["resourceBody"] = Value::Object(body);
                        }
                    }
                    entry
                }
            })
            .collect();
        let status = if any_failed { StatusCode::MULTI_STATUS } else { StatusCode::OK };
        Ok(ok(status, Value::Array(entries)))
    })
}

/// The throughput offers: one per database or container that has throughput.
fn offer_list(engine: &Engine, req: &Req) -> Result<Vec<Value>, Failure> {
    with_account(engine, req, |a| {
        let mut offers = Vec::new();
        let mut add = |rid: &str, link: String, throughput: i64| {
            offers.push(json!({
                "id": rid,
                "_rid": rid,
                "_self": format!("offers/{rid}/"),
                "resource": link,
                "offerResourceId": rid,
                "offerType": "Invalid",
                "offerVersion": "V2",
                "content": {"offerThroughput": throughput, "offerIsRUPerMinuteThroughputEnabled": false},
                "_etag": store::new_etag(),
                "_ts": store::now(),
            }));
        };
        for database in a.databases.values() {
            if let Some(t) = database.throughput {
                add(&database.system.rid, format!("dbs/{}/", database.system.rid), t);
            }
            for container in database.containers.values() {
                if let Some(t) = container.throughput {
                    add(&container.system.rid, format!("dbs/{}/colls/{}/", database.system.rid, container.system.rid), t);
                }
            }
        }
        Ok(offers)
    })
}

fn offers(engine: &Engine, req: &Req) -> Result<Response2, Failure> {
    let offers = offer_list(engine, req)?;
    feed(req, "", "Offers", filter_listing(req, offers)?)
}

fn offer(engine: &Engine, req: &Req) -> Result<Response2, Failure> {
    let rid = req.segments[1].clone();
    if req.method == Method::PUT {
        let body = body_object(req)?;
        let throughput = body.get("content").and_then(|c| c.get("offerThroughput")).and_then(Value::as_i64).ok_or_else(|| bad_request("The offer needs content.offerThroughput."))?;
        with_account(engine, req, |a| {
            for database in a.databases.values_mut() {
                if database.system.rid == rid {
                    database.throughput = Some(throughput);
                }
                for container in database.containers.values_mut() {
                    if container.system.rid == rid {
                        container.throughput = Some(throughput);
                    }
                }
            }
            Ok(())
        })?;
    }
    let found = offer_list(engine, req)?.into_iter().find(|o| o["id"] == json!(rid)).ok_or_else(|| not_found(&rid))?;
    Ok(ok(StatusCode::OK, found))
}

/// Whether an account by this name has a Resource Manager resource.
pub(crate) fn account_resource_id(engine: &Engine, account: &str) -> Option<String> {
    let state = engine.state::<ArmState>();
    let state = state.lock();
    let id = state
        .resources()
        .find(|r| r.resource_type.eq_ignore_ascii_case(ACCOUNT_TYPE) && r.name.eq_ignore_ascii_case(account))
        .map(|r| r.id.clone());
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patches() {
        let mut doc = json!({"id": "1", "n": 1, "tags": ["a"], "nested": {"x": 1}});
        for op in [
            json!({"op": "incr", "path": "/n", "value": 2}),
            json!({"op": "add", "path": "/tags/-", "value": "b"}),
            json!({"op": "add", "path": "/tags/0", "value": "z"}),
            json!({"op": "set", "path": "/nested/y", "value": true}),
            json!({"op": "remove", "path": "/nested/x"}),
            json!({"op": "move", "from": "/nested", "path": "/moved"}),
        ] {
            apply_patch(&mut doc, op.as_object().unwrap()).unwrap();
        }
        assert_eq!(doc, json!({"id": "1", "n": 3, "tags": ["z", "a", "b"], "moved": {"y": true}}));
        assert!(apply_patch(&mut doc, json!({"op": "replace", "path": "/missing", "value": 1}).as_object().unwrap()).is_err());
    }
}
