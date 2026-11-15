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
    /// Each one is instead sent, with placeholder names and an empty body, to a
    /// scratch emulator (same configuration, its own state) holding the
    /// resource the data plane belongs to: it counts as implemented unless
    /// the answer is `501`.
    pub async fn data_plane_coverage(&self) -> Result<Vec<DataPlaneCoverage>, String> {
        use tower::ServiceExt;
        let scratch = Emulator::new(self.config().clone());
        let config = scratch.config();
        let management = format!("{}:{}", config.host, config.port);
        let arm_token = emulator_core::auth::issue_token(config, "https://management.azure.com/", emulator_core::auth::PROVISIONING_CLIENT);
        let send = |method: &str, host: &str, path: String, token: &str, body: Option<String>| {
            let request = Request::builder()
                .method(method)
                .uri(path)
                .header("host", host)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(body.map(Body::from).unwrap_or_else(Body::empty));
            let router = scratch.router();
            async move {
                let request = request.map_err(|e| e.to_string())?;
                router.oneshot(request).await.map(|response| response.status()).map_err(|e| e.to_string())
            }
        };

        // Key Vault: a vault to address the operations to.
        let group = format!("/subscriptions/{}/resourceGroups/coverage", config.subscription_id);
        let vault = serde_json::json!({
            "location": "eastus",
            "properties": {"tenantId": config.tenant_id, "sku": {"family": "A", "name": "standard"}, "accessPolicies": []},
        });
        let setup = [
            (format!("{group}?api-version=2021-04-01"), r#"{"location":"eastus"}"#.to_string()),
            (format!("{group}/providers/Microsoft.KeyVault/vaults/coverage?api-version=2023-07-01"), vault.to_string()),
        ];
        for (path, body) in setup {
            let status = send("PUT", &management, path.clone(), &arm_token, Some(body)).await?;
            if !status.is_success() {
                return Err(format!("PUT {path} answered {status}"));
            }
        }
        let plane = &service_keyvault::data_scaffold::DATA_PLANE;
        let vault_uri = config.vault_uri("coverage");
        let host = vault_uri.split("://").nth(1).unwrap_or(&vault_uri).trim_end_matches('/').to_string();
        let token = emulator_core::auth::issue_token(config, &config.vault_resource(), emulator_core::auth::PROVISIONING_CLIENT);
        let mut coverage = Vec::new();
        for operation in plane.operations {
            // Every `{parameter}` becomes a name that is valid anywhere.
            let path: Vec<&str> = operation.path.split('/').map(|s| if s.starts_with('{') { "coverage" } else { s }).collect();
            let path = format!("{}?api-version={}", path.join("/"), plane.api_version);
            let body = matches!(operation.method, "PUT" | "POST" | "PATCH").then(|| "{}".to_string());
            let status = send(operation.method, &host, path, &token, body).await?;
            coverage.push(DataPlaneCoverage { plane: plane.name, operation, implemented: status != http::StatusCode::NOT_IMPLEMENTED });
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
