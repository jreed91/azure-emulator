//! `Microsoft.ManagedIdentity` resource provider.
//!
//! Every resource type in the provider's API specification is mounted from
//! [`scaffold::PROVIDER`] and answers `501` until it is implemented. To implement a
//! type, write a `ResourceTypeHandler` for it and register it here, before the
//! scaffold. `CONTRACT.md` describes what the specification says about each type.
//!
//! Implemented: user-assigned identities and their federated identity credentials,
//! the resources that use a user-assigned identity, and reading any resource's
//! system-assigned identity. Identities on resources themselves are filled in by
//! the ARM layer (`emulator_core::identity`). Revoking an identity's tokens is
//! not implemented.
//! The code and wording of validation refusals have not been checked against Azure.

pub mod scaffold;

use std::sync::Arc;

use emulator_core::arm::{ArmContext, ArmRegistry, ArmState, OperationRequest, PutRequest, Resource, ResourceTypeHandler};
use emulator_core::ArmError;
use serde_json::{json, Value};

/// Mounts the provider on Resource Manager.
pub fn register(registry: &mut ArmRegistry) {
    registry.register_type(UserAssignedIdentities);
    registry.register_type(FederatedIdentityCredentials);
    registry.register_operation("Microsoft.ManagedIdentity/SystemAssignedIdentities_GetByScope", Arc::new(system_assigned_by_scope));
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

    fn actions(&self) -> &'static [&'static str] {
        &["listAssociatedResources"]
    }

    fn action(&self, ctx: &ArmContext<'_>, resource: &Resource, action: &str, _body: &Value) -> Result<Value, ArmError> {
        if !action.eq_ignore_ascii_case("listAssociatedResources") {
            return Err(ArmError::not_implemented(format_args!("the '{action}' action on {}", self.resource_type())));
        }
        // Every resource whose identity block names this identity.
        let state = ctx.engine.state::<ArmState>();
        let state = state.lock();
        let users: Vec<Value> = state
            .resources()
            .filter(|r| {
                r.identity
                    .as_ref()
                    .and_then(|i| i.get("userAssignedIdentities"))
                    .and_then(Value::as_object)
                    .is_some_and(|ids| ids.keys().any(|id| id.eq_ignore_ascii_case(&resource.id)))
            })
            .map(|r| {
                let group = r.id.split('/').nth(4).unwrap_or("");
                json!({
                    "id": r.id,
                    "name": r.name,
                    "type": r.resource_type,
                    "resourceGroup": group,
                    "subscriptionId": ctx.subscription_id,
                    "subscriptionDisplayName": "Azure Emulator",
                })
            })
            .collect();
        Ok(json!({"totalCount": users.len(), "value": users}))
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

struct FederatedIdentityCredentials;

fn invalid(message: impl Into<String>) -> ArmError {
    ArmError::bad_request("BadRequest", message)
}

impl ResourceTypeHandler for FederatedIdentityCredentials {
    fn resource_type(&self) -> &'static str {
        "Microsoft.ManagedIdentity/userAssignedIdentities/federatedIdentityCredentials"
    }

    fn put(&self, _ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        // The specification's pattern: ^[a-zA-Z0-9]{1}[a-zA-Z0-9-_]{2,119}$
        let name = request.name.as_bytes();
        let valid_name = (3..=120).contains(&name.len())
            && name[0].is_ascii_alphanumeric()
            && name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_');
        if !valid_name {
            return Err(invalid(format!(
                "The federated identity credential name '{}' is invalid. It must be 3 to 120 characters, start with a letter or number, and contain only letters, numbers, hyphens and underscores.",
                request.name
            )));
        }
        let given = |member: &str| {
            request
                .body
                .pointer(&format!("/properties/{member}"))
                .filter(|v| !v.is_null())
        };
        let issuer = given("issuer")
            .and_then(Value::as_str)
            .filter(|issuer| !issuer.is_empty())
            .ok_or_else(|| invalid("The required property 'properties.issuer' is missing."))?;
        let audiences = given("audiences")
            .and_then(Value::as_array)
            .filter(|list| !list.is_empty() && list.iter().all(Value::is_string))
            .ok_or_else(|| invalid("The required property 'properties.audiences' must be a non-empty list of strings."))?;
        let mut properties = json!({"issuer": issuer, "audiences": audiences});
        // The external identity is named either exactly or by an expression.
        match (given("subject"), given("claimsMatchingExpression")) {
            (Some(subject), None) => properties["subject"] = subject.clone(),
            (None, Some(expression)) => properties["claimsMatchingExpression"] = expression.clone(),
            _ => {
                return Err(invalid(
                    "Exactly one of 'properties.subject' and 'properties.claimsMatchingExpression' must be set.",
                ))
            }
        }
        Ok(properties)
    }
}

/// The system-assigned identity of the resource at `scope`.
fn system_assigned_by_scope(
    ctx: &ArmContext<'_>,
    request: &OperationRequest<'_>,
) -> Result<(http::StatusCode, Value), ArmError> {
    let scope = request.param("scope");
    let state = ctx.engine.state::<ArmState>();
    let state = state.lock();
    let missing = || {
        ArmError::not_found(
            "ResourceNotFound",
            format!("The resource '{scope}' was not found or has no system-assigned identity."),
        )
    };
    let resource = state.resource(scope).ok_or_else(missing)?;
    let identity = resource.identity.as_ref().ok_or_else(missing)?;
    let principal = identity.get("principalId").and_then(Value::as_str).ok_or_else(missing)?;
    let mut out = json!({
        "id": format!("{}/providers/Microsoft.ManagedIdentity/identities/default", resource.id),
        "name": "default",
        "type": "Microsoft.ManagedIdentity/identities",
        "properties": {
            "tenantId": identity.get("tenantId"),
            "principalId": principal,
            "clientId": state.system_identity_client(principal),
        },
    });
    if let Some(location) = &resource.location {
        out["location"] = json!(location);
    }
    Ok((http::StatusCode::OK, out))
}
