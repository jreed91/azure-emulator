//! The operations of a data plane, as its API specification lists them.
//!
//! Data planes are hand-written functions from a request to a response, so
//! nothing mounts these tables. They exist so that `azure-emulator coverage`
//! can say which of a data plane's operations the emulator answers and which
//! it refuses with `501`, as it does for Resource Manager.

/// One operation of a data plane.
pub struct DataOperation {
    /// The specification's `operationId`.
    pub id: &'static str,
    pub method: &'static str,
    /// The path template, with `{parameter}` segments.
    pub path: &'static str,
}

/// A data plane's operations, generated from its specification by
/// `emulator-scaffold data-plane`.
pub struct DataPlane {
    /// What the data plane is called, such as `Key Vault`.
    pub name: &'static str,
    /// The `api-version` the operations were read from.
    pub api_version: &'static str,
    pub operations: &'static [DataOperation],
}
