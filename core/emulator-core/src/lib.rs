//! Shared framework for the Azure emulator: the in-memory engine, configuration,
//! token issuing and verification, and the Azure Resource Manager (ARM) layer every
//! resource provider plugs into.

pub mod arm;
pub mod auth;
pub mod config;
pub mod engine;
pub mod error;
pub mod http_util;
pub mod identity;
pub mod schema;
pub mod trace;

pub use config::Config;
pub use engine::Engine;
pub use error::ArmError;
