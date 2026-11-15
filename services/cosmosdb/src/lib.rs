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
    // Created only by Microsoft.Network, which the emulator does not model.
    registry.register_type(emulator_core::arm::CreatedByNetwork("Microsoft.DocumentDB/databaseAccounts/privateEndpointConnections"));
    registry.register_type(emulator_core::arm::CreatedByNetwork("Microsoft.DocumentDB/databaseAccounts/networkSecurityPerimeterConfigurations"));
    registry.register_generic_remaining(&scaffold::PROVIDER, GENERIC_EXCEPTIONS);
    registry.register_scaffold(&scaffold::PROVIDER);
}

/// The configuration-shaped types that are not served from their schema, and why.
const GENERIC_EXCEPTIONS: &[(&str, &str)] = &[
    ("Microsoft.DocumentDB/databaseAccounts/apis/databases", "the 2015 API's path, under '/apis/{api}', which is not a resource"),
    ("Microsoft.DocumentDB/databaseAccounts/apis/databases/collections", "the 2015 API's path, under '/apis/{api}', which is not a resource"),
    ("Microsoft.DocumentDB/databaseAccounts/apis/databases/collections/settings", "the 2015 API's path, under '/apis/{api}', which is not a resource"),
    ("Microsoft.DocumentDB/databaseAccounts/apis/databases/containers", "the 2015 API's path, under '/apis/{api}', which is not a resource"),
    ("Microsoft.DocumentDB/databaseAccounts/apis/databases/containers/settings", "the 2015 API's path, under '/apis/{api}', which is not a resource"),
    ("Microsoft.DocumentDB/databaseAccounts/apis/databases/graphs", "the 2015 API's path, under '/apis/{api}', which is not a resource"),
    ("Microsoft.DocumentDB/databaseAccounts/apis/databases/graphs/settings", "the 2015 API's path, under '/apis/{api}', which is not a resource"),
    ("Microsoft.DocumentDB/databaseAccounts/apis/databases/settings", "the 2015 API's path, under '/apis/{api}', which is not a resource"),
    ("Microsoft.DocumentDB/databaseAccounts/apis/keyspaces", "the 2015 API's path, under '/apis/{api}', which is not a resource"),
    ("Microsoft.DocumentDB/databaseAccounts/apis/keyspaces/settings", "the 2015 API's path, under '/apis/{api}', which is not a resource"),
    ("Microsoft.DocumentDB/databaseAccounts/apis/keyspaces/tables", "the 2015 API's path, under '/apis/{api}', which is not a resource"),
    ("Microsoft.DocumentDB/databaseAccounts/apis/keyspaces/tables/settings", "the 2015 API's path, under '/apis/{api}', which is not a resource"),
    ("Microsoft.DocumentDB/databaseAccounts/apis/tables", "the 2015 API's path, under '/apis/{api}', which is not a resource"),
    ("Microsoft.DocumentDB/databaseAccounts/apis/tables/settings", "the 2015 API's path, under '/apis/{api}', which is not a resource"),
    ("Microsoft.DocumentDB/databaseAccounts/cassandraRoleAssignments", "access control for a data plane; stored without being enforced, it would look like protection"),
    ("Microsoft.DocumentDB/databaseAccounts/cassandraRoleDefinitions", "access control for a data plane; stored without being enforced, it would look like protection"),
    ("Microsoft.DocumentDB/databaseAccounts/gremlinRoleAssignments", "access control for a data plane; stored without being enforced, it would look like protection"),
    ("Microsoft.DocumentDB/databaseAccounts/gremlinRoleDefinitions", "access control for a data plane; stored without being enforced, it would look like protection"),
    ("Microsoft.DocumentDB/databaseAccounts/mongoMIRoleAssignments", "access control for a data plane; stored without being enforced, it would look like protection"),
    ("Microsoft.DocumentDB/databaseAccounts/mongoMIRoleDefinitions", "access control for a data plane; stored without being enforced, it would look like protection"),
    ("Microsoft.DocumentDB/databaseAccounts/mongodbRoleDefinitions", "access control for a data plane; stored without being enforced, it would look like protection"),
    ("Microsoft.DocumentDB/databaseAccounts/mongodbUserDefinitions", "access control for a data plane; stored without being enforced, it would look like protection"),
    ("Microsoft.DocumentDB/databaseAccounts/sqlRoleAssignments", "access control for a data plane; stored without being enforced, it would look like protection"),
    ("Microsoft.DocumentDB/databaseAccounts/sqlRoleDefinitions", "access control for a data plane; stored without being enforced, it would look like protection"),
    ("Microsoft.DocumentDB/databaseAccounts/tableRoleAssignments", "access control for a data plane; stored without being enforced, it would look like protection"),
    ("Microsoft.DocumentDB/databaseAccounts/tableRoleDefinitions", "access control for a data plane; stored without being enforced, it would look like protection"),
    ("Microsoft.DocumentDB/databaseAccounts/chaosFaults", "injects faults into the account; storing the setting would report one that is not happening"),
    ("Microsoft.DocumentDB/databaseAccounts/sqlDatabases/clientEncryptionKeys", "keys the data plane would have to encrypt with, and does not"),
];
