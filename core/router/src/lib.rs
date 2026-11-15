//! The emulator: every service mounted behind one endpoint.
//!
//! Requests are routed on the `Host` header — see [`emulator_core::config`] — so one
//! port serves Resource Manager, the token service and every data plane.
//!
//! # Embedded use
//!
//! ```
//! let emulator = azure_emulator::Emulator::new(azure_emulator::Config::default());
//! let _service = emulator.router(); // a `tower::Service` to drive in-process
//! ```
//!
//! The router answers `GET /__admin/health` on any host, and turns a panic in
//! a handler into a 500 answer (see [`catch_panic`]) so one bad request never
//! takes the connection, or the emulator, down with it.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::task::Poll;

use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use axum::Router;
use emulator_core::arm::{self, ArmRegistry};
use emulator_core::http_util::request_host;
use emulator_core::Engine;
use http::Request;

pub use emulator_core::Config;

/// The emulator's version: `AZURE_EMULATOR_VERSION` at build time when set
/// (release builds stamp the tag), else the crate version.
pub const VERSION: &str = match option_env!("AZURE_EMULATOR_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

/// The readiness endpoint, answered on every host without authentication.
pub const HEALTH_PATH: &str = "/__admin/health";

pub mod idle;
pub mod persist;
pub mod provision;
pub mod tls;

/// One data-plane operation and whether this emulator answers it.
pub struct DataPlaneCoverage {
    /// The data plane's name, such as `Key Vault`.
    pub plane: &'static str,
    pub operation: &'static emulator_core::dataplane::DataOperation,
    pub implemented: bool,
}

/// One isolated emulator instance.
#[derive(Clone)]
pub struct Emulator {
    engine: Arc<Engine>,
    registry: Arc<ArmRegistry>,
}

impl Emulator {
    pub fn new(config: Config) -> Self {
        let mut registry = ArmRegistry::new();
        service_storage::register(&mut registry);
        service_keyvault::register(&mut registry);
        service_managedidentity::register(&mut registry);
        service_servicebus::register(&mut registry);
        service_cosmosdb::register(&mut registry);
        service_eventhub::register(&mut registry);
        // emulator-scaffold: new services are registered above this line.
        Self {
            engine: Arc::new(Engine::new(config)),
            registry: Arc::new(registry),
        }
    }

    /// The shared engine, for serving data planes outside the HTTP router.
    pub fn engine(&self) -> Arc<Engine> {
        self.engine.clone()
    }

    pub fn config(&self) -> &Config {
        self.engine.config()
    }

    /// Every operation in the mounted providers' specifications, and whether this
    /// emulator implements it.
    pub fn coverage(&self) -> Vec<arm::OperationCoverage> {
        self.registry.coverage()
    }

    /// Every operation in the data planes that have a generated operation
    /// table, and whether this emulator answers it.
    ///
    /// Data planes are hand-written, so nothing registers their operations.
    /// Each one is instead sent to a scratch emulator (same configuration, its
    /// own state) holding resources named `coverage` for it to address, with
    /// the query and header values the specification says select it,
    /// `coverage` for every other required one, and an empty body. It counts
    /// as implemented unless the answer is `501`. This measures routing, not
    /// correctness: two operations that differ only in an optional parameter
    /// get the same answer.
    pub async fn data_plane_coverage(&self) -> Result<Vec<DataPlaneCoverage>, String> {
        use tower::ServiceExt;
        type Headers = Vec<(&'static str, String)>;
        // A request a probe sends: method, path and query, headers, body.
        type Call = (&'static str, String, Headers, &'static str);
        struct Probe {
            plane: &'static emulator_core::dataplane::DataPlane,
            /// The data plane's endpoint for the resource named `coverage`.
            endpoint: String,
            token: String,
            /// Sent with every request, the version among them.
            headers: Headers,
            /// The version as a query parameter, where the data plane takes it there.
            version_query: bool,
            /// Sent with every request that has a body.
            body: &'static str,
            /// Requests that put back what an earlier probe may have removed.
            setup: Vec<Call>,
        }

        let scratch = Emulator::new(self.config().clone());
        let config = scratch.config();
        let send = |host: String, token: &str, call: Call| {
            let (method, path, headers, body) = call;
            let mut request = Request::builder().method(method).uri(path).header("host", host).header("authorization", format!("Bearer {token}"));
            for (name, value) in headers {
                request = request.header(name, value);
            }
            let request = request.body(if body.is_empty() { Body::empty() } else { Body::from(body) });
            let router = scratch.router();
            async move {
                // The router cannot fail: its error type has no values.
                match router.oneshot(request.map_err(|e| e.to_string())?).await {
                    Ok(response) => Ok::<_, String>(response.status()),
                    Err(never) => match never {},
                }
            }
        };
        let host_of = |endpoint: &str| endpoint.split("://").nth(1).unwrap_or(endpoint).trim_end_matches('/').to_string();

        // The resources the data planes belong to.
        let management = format!("{}:{}", config.host, config.port);
        let arm_token = emulator_core::auth::issue_token(config, "https://management.azure.com/", emulator_core::auth::PROVISIONING_CLIENT);
        let group = format!("/subscriptions/{}/resourceGroups/coverage", config.subscription_id);
        let vault = serde_json::json!({
            "location": "eastus",
            "properties": {"tenantId": config.tenant_id, "sku": {"family": "A", "name": "standard"}, "accessPolicies": []},
        });
        let account = serde_json::json!({"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}});
        let resources = [
            (format!("{group}?api-version=2021-04-01"), r#"{"location":"eastus"}"#.to_string()),
            (format!("{group}/providers/Microsoft.KeyVault/vaults/coverage?api-version=2023-07-01"), vault.to_string()),
            (format!("{group}/providers/Microsoft.Storage/storageAccounts/coverage?api-version=2023-05-01"), account.to_string()),
        ];
        for (path, body) in resources {
            let body: &'static str = body.leak();
            let json = vec![("content-type", "application/json".to_string())];
            let status = send(management.clone(), &arm_token, ("PUT", path.clone(), json, body)).await?;
            if !status.is_success() {
                return Err(format!("PUT {path} answered {status}"));
            }
        }

        let vault_token = emulator_core::auth::issue_token(config, &config.vault_resource(), emulator_core::auth::PROVISIONING_CLIENT);
        let storage_token = emulator_core::auth::issue_token(config, "https://storage.azure.com", emulator_core::auth::PROVISIONING_CLIENT);
        let json = || vec![("content-type", "application/json".to_string())];
        let storage = |plane: &'static emulator_core::dataplane::DataPlane, extra: Headers| -> Headers {
            let now = httpdate::fmt_http_date(std::time::SystemTime::now());
            [vec![("x-ms-version", plane.api_version.to_string()), ("x-ms-date", now)], extra].concat()
        };
        let blob = &service_storage::blob_scaffold::DATA_PLANE;
        let queue = &service_storage::queue_scaffold::DATA_PLANE;
        let files = &service_storage::files_scaffold::DATA_PLANE;
        let table = &service_storage::table_scaffold::DATA_PLANE;
        let file_intent = || vec![("x-ms-file-request-intent", "backup".to_string())];
        let probes = [
            Probe {
                plane: &service_keyvault::data_scaffold::DATA_PLANE,
                endpoint: config.vault_uri("coverage"),
                token: vault_token,
                headers: json(),
                version_query: true,
                body: "{}",
                setup: Vec::new(),
            },
            Probe {
                plane: blob,
                endpoint: config.blob_endpoint("coverage"),
                token: storage_token.clone(),
                headers: storage(blob, Vec::new()),
                version_query: false,
                body: "",
                setup: vec![
                    ("PUT", "/coverage?restype=container".into(), Vec::new(), ""),
                    ("PUT", "/coverage/coverage".into(), vec![("x-ms-blob-type", "BlockBlob".into())], "coverage"),
                ],
            },
            Probe {
                plane: queue,
                endpoint: config.queue_endpoint("coverage"),
                token: storage_token.clone(),
                headers: storage(queue, Vec::new()),
                version_query: false,
                body: "",
                setup: vec![("PUT", "/coverage".into(), Vec::new(), "")],
            },
            Probe {
                plane: files,
                endpoint: config.file_endpoint("coverage"),
                token: storage_token.clone(),
                headers: storage(files, file_intent()),
                version_query: false,
                body: "",
                setup: vec![
                    ("PUT", "/coverage?restype=share".into(), Vec::new(), ""),
                    ("PUT", "/coverage/coverage?restype=directory".into(), Vec::new(), ""),
                    (
                        "PUT",
                        "/coverage/coverage/coverage".into(),
                        vec![("x-ms-type", "file".into()), ("x-ms-content-length", "8".into())],
                        "",
                    ),
                ],
            },
            Probe {
                plane: table,
                endpoint: config.table_endpoint("coverage"),
                token: storage_token,
                headers: storage(table, json()),
                version_query: false,
                body: "{}",
                setup: vec![("POST", "/Tables".into(), Vec::new(), r#"{"TableName":"coverage"}"#)],
            },
        ];

        let mut coverage = Vec::new();
        for probe in &probes {
            let host = host_of(&probe.endpoint);
            for operation in probe.plane.operations {
                for (method, path, extra, body) in &probe.setup {
                    let headers = [probe.headers.clone(), extra.clone()].concat();
                    send(host.clone(), &probe.token, (method, path.clone(), headers, body)).await?;
                }
                // Every `{parameter}` becomes a name that is valid anywhere.
                let mut path = String::new();
                let mut in_parameter = false;
                for c in operation.path.chars() {
                    match c {
                        '{' => {
                            in_parameter = true;
                            path.push_str("coverage");
                        }
                        '}' => in_parameter = false,
                        c if !in_parameter => path.push(c),
                        _ => {}
                    }
                }
                let value = |value: &'static str| if value.is_empty() { "coverage" } else { value };
                let mut query: Vec<String> = Vec::new();
                if probe.version_query {
                    query.push(format!("api-version={}", probe.plane.api_version));
                }
                let mut headers = probe.headers.clone();
                for (place, name, fixed) in operation.fixed {
                    match *place {
                        "query" => query.push(format!("{name}={}", value(fixed))),
                        _ => headers.push((name, value(fixed).to_string())),
                    }
                }
                if !query.is_empty() {
                    path = format!("{path}?{}", query.join("&"));
                }
                let body = if matches!(operation.method, "PUT" | "POST" | "PATCH") { probe.body } else { "" };
                let status = send(host.clone(), &probe.token, (operation.method, path.clone(), headers, body)).await?;
                // A refused token would make everything look implemented.
                if matches!(status, http::StatusCode::UNAUTHORIZED | http::StatusCode::FORBIDDEN) {
                    return Err(format!("{}: {} {path} answered {status}", probe.plane.name, operation.method));
                }
                coverage.push(DataPlaneCoverage {
                    plane: probe.plane.name,
                    operation,
                    implemented: status != http::StatusCode::NOT_IMPLEMENTED,
                });
            }
        }
        Ok(coverage)
    }

    /// The HTTP service for this instance.
    pub fn router(&self) -> Router {
        Router::new().fallback(guarded_dispatch).with_state(self.clone())
    }
}

async fn guarded_dispatch(State(emulator): State<Emulator>, request: Request<Body>) -> Response {
    let what = format!("{} {}", request.method(), request.uri().path());
    catch_panic(&what, dispatch(emulator, request)).await
}

/// Runs a handler's `future`, turning a panic in it into a 500 answer with an
/// Azure error envelope. The panic is logged to stderr (with `what`, such as
/// the method and path); the client sees only a short, fixed message.
pub async fn catch_panic<F: Future<Output = Response>>(what: &str, future: F) -> Response {
    let mut future = std::pin::pin!(future);
    let outcome = std::future::poll_fn(|context| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(context))) {
            Ok(Poll::Ready(response)) => Poll::Ready(Ok(response)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => Poll::Ready(Err(payload)),
        }
    })
    .await;
    match outcome {
        Ok(response) => response,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("(no message)");
            eprintln!("error: the handler for {what} panicked: {message}; answered 500");
            let body = serde_json::json!({"error": {
                "code": "InternalServerError",
                "message": "The emulator failed while handling this request. Its log has the details.",
            }});
            json_response(http::StatusCode::INTERNAL_SERVER_ERROR, &body)
        }
    }
}

fn json_response(status: http::StatusCode, body: &serde_json::Value) -> Response {
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("application/json; charset=utf-8"));
    response
}

async fn dispatch(emulator: Emulator, request: Request<Body>) -> Response {
    if request.uri().path() == HEALTH_PATH && matches!(*request.method(), http::Method::GET | http::Method::HEAD) {
        return json_response(http::StatusCode::OK, &serde_json::json!({"status": "ok", "version": VERSION}));
    }
    let engine = emulator.engine.clone();
    let host = request_host(request.headers(), request.uri());
    let config = engine.config();
    if let Some(account) = host.strip_suffix(&config.blob_host_suffix()) {
        return service_storage::blob::handle(engine.clone(), account.to_string(), request).await;
    }
    if let Some(account) = host.strip_suffix(&config.file_host_suffix()) {
        return service_storage::files::handle(engine.clone(), account.to_string(), request).await;
    }
    if let Some(account) = host.strip_suffix(&config.table_host_suffix()) {
        return service_storage::table::handle(engine.clone(), account.to_string(), request).await;
    }
    if let Some(account) = host.strip_suffix(&config.queue_host_suffix()) {
        return service_storage::queue::handle(engine.clone(), account.to_string(), request).await;
    }
    if let Some(vault) = host.strip_suffix(&config.vault_host_suffix()) {
        return service_keyvault::secrets::handle(engine.clone(), vault.to_string(), request).await;
    }
    if service_entra::matches(request.uri().path()) {
        return service_entra::handle(engine, request).await;
    }
    if host.ends_with(&format!(".documents.{}", config.dns_suffix)) || service_cosmosdb::data::is_cosmos_request(request.headers()) {
        let host = request.headers().get(http::header::HOST).and_then(|h| h.to_str().ok()).unwrap_or(&host).to_string();
        return service_cosmosdb::data::handle(engine, host, request).await;
    }
    if request.uri().path() == "/$servicebus/websocket" {
        return servicebus_websocket(engine, request).await;
    }
    arm::handle(engine, emulator.registry.clone(), request).await
}

/// AMQP over WebSockets, as the Service Bus SDKs speak it when told to (or
/// when a firewall allows only 443): binary messages carry the AMQP byte
/// stream, which is handed to the same broker as the AMQP listeners.
async fn servicebus_websocket(engine: Arc<Engine>, request: Request<Body>) -> Response {
    use axum::extract::ws::{Message, WebSocketUpgrade};
    use axum::extract::FromRequestParts;
    use axum::response::IntoResponse;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut parts, _) = request.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    upgrade
        .protocols(["AMQPWSB10"])
        .on_upgrade(move |mut socket| async move {
            let (broker_side, bridge_side) = tokio::io::duplex(256 * 1024);
            tokio::spawn(service_servicebus::broker::serve_stream(engine, broker_side));
            let (mut from_broker, mut to_broker) = tokio::io::split(bridge_side);
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                tokio::select! {
                    message = socket.recv() => match message {
                        Some(Ok(Message::Binary(bytes))) => {
                            if to_broker.write_all(&bytes).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                        Some(Ok(_)) => {}
                    },
                    read = from_broker.read(&mut buffer) => match read {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if socket.send(Message::Binary(buffer[..n].to_vec().into())).await.is_err() {
                                break;
                            }
                        }
                    },
                }
            }
        })
        .into_response()
}
