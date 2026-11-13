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
    if let Some(vault) = host.strip_suffix(&config.vault_host_suffix()) {
        return service_keyvault::secrets::handle(engine.clone(), vault.to_string(), request).await;
    }
    if service_entra::matches(request.uri().path()) {
        return service_entra::handle(engine, request).await;
    }
    arm::handle(engine, emulator.registry.clone(), request).await
}
