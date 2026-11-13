//! `Microsoft.DocumentDB`: Cosmos DB accounts on Resource Manager, and the
//! Cosmos DB for NoSQL data plane ([`data`]).
//!
//! Every other resource type in the provider's API specification is mounted
//! from [`scaffold::PROVIDER`] and answers `501` until it is implemented.
//! `CONTRACT.md` describes what the specification says about each type.

pub(crate) mod accounts;
pub(crate) mod arm_sync;
pub mod data;
pub mod query;
pub mod scaffold;
pub(crate) mod store;

use emulator_core::arm::ArmRegistry;

pub use store::CosmosState;

/// Mounts the provider on Resource Manager.
pub fn register(registry: &mut ArmRegistry) {
    registry.register_type(accounts::DatabaseAccounts);
    registry.register_type(accounts::SqlDatabases);
    registry.register_type(accounts::SqlContainers);
    registry.register_scaffold(&scaffold::PROVIDER);
}
