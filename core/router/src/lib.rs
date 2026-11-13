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

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use axum::Router;
use emulator_core::arm::{self, ArmRegistry};
use emulator_core::http_util::request_host;
use emulator_core::Engine;
use http::Request;

pub use emulator_core::Config;

pub mod persist;
pub mod provision;
pub mod tls;

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

    /// The HTTP service for this instance.
    pub fn router(&self) -> Router {
        Router::new().fallback(dispatch).with_state(self.clone())
    }
}

async fn dispatch(State(emulator): State<Emulator>, request: Request<Body>) -> Response {
    let engine = emulator.engine.clone();
    let host = request_host(request.headers(), request.uri());
    let config = engine.config();
    if let Some(account) = host.strip_suffix(&config.blob_host_suffix()) {
        return service_storage::blob::handle(engine.clone(), account.to_string(), request).await;
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
