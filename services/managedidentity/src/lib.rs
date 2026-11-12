//! `Microsoft.ManagedIdentity` resource provider.
//!
//! Every resource type in the provider's API specification is mounted from
//! [`scaffold::PROVIDER`] and answers `501` until it is implemented. To implement a
//! type, write a `ResourceTypeHandler` for it and register it here, before the
//! scaffold. `CONTRACT.md` describes what the specification says about each type.
//!
//! Implemented: user-assigned identities. Federated identity credentials are stubs.

pub mod scaffold;

use emulator_core::arm::{ArmContext, ArmRegistry, PutRequest, ResourceTypeHandler};
use emulator_core::ArmError;
use serde_json::{json, Value};

/// Mounts the provider on Resource Manager.
pub fn register(registry: &mut ArmRegistry) {
    registry.register_type(UserAssignedIdentities);
    registry.register_scaffold(&scaffold::PROVIDER);
}

struct UserAssignedIdentities;

impl ResourceTypeHandler for UserAssignedIdentities {
    fn resource_type(&self) -> &'static str {
        "Microsoft.ManagedIdentity/userAssignedIdentities"
    }

    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        // The service assigns these once; an update keeps them.
        let mut properties = match request.existing {
            Some(existing) => existing.properties.clone(),
            None => json!({
                "tenantId": ctx.engine.config().tenant_id,
                "principalId": uuid::Uuid::new_v4().to_string(),
                "clientId": uuid::Uuid::new_v4().to_string(),
            }),
        };
        // The settable properties the specification defines pass through.
        for member in ["isolationScope", "assignmentRestrictions"] {
            if let Some(value) = request.body.pointer(&format!("/properties/{member}")) {
                properties[member] = value.clone();
            }
        }
        Ok(properties)
    }

    fn patch(
        &self,
        _ctx: &ArmContext<'_>,
        existing: &emulator_core::arm::Resource,
        body: &Value,
    ) -> Result<Value, ArmError> {
        // The identifiers are read-only: only the settable properties change.
        let mut properties = existing.properties.clone();
        for member in ["isolationScope", "assignmentRestrictions"] {
            if let Some(value) = body.pointer(&format!("/properties/{member}")) {
                properties[member] = value.clone();
            }
        }
        Ok(properties)
    }
}
