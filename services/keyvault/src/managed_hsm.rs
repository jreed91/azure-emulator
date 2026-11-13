//! Managed HSMs on Resource Manager: the resource, its soft delete, name
//! availability, regions and private link resources.
//!
//! The HSM's data plane is not served: `hsmUri` points at the emulator's domain,
//! and the security domain is never activated.

use std::sync::Arc;

use emulator_core::arm::{
    ArmContext, ArmRegistry, ArmState, OperationRequest, PutRequest, Resource, ResourceTypeHandler,
};
use emulator_core::ArmError;
use http::StatusCode;
use serde_json::{json, Value};

use crate::{DeletedHsm, KeyVaultState};

const HSM_TYPE: &str = "Microsoft.KeyVault/managedHSMs";

pub(crate) fn register(registry: &mut ArmRegistry) {
    registry.register_type(ManagedHsms);
    registry.register_provider_action(
        "Microsoft.KeyVault/checkMhsmNameAvailability",
        Arc::new(check_name_availability),
    );
    registry.register_operation("ManagedHsms_ListDeleted", Arc::new(list_deleted));
    registry.register_operation("ManagedHsms_GetDeleted", Arc::new(get_deleted));
    registry.register_operation("ManagedHsms_PurgeDeleted", Arc::new(purge_deleted));
    registry.register_operation("MHSMRegions_ListByResource", Arc::new(regions));
    registry.register_operation("MHSMPrivateLinkResources_ListByMHSMResource", Arc::new(private_link_resources));
}

fn invalid(message: impl Into<String>) -> ArmError {
    ArmError::bad_request("BadRequest", message)
}

/// 3 to 24 letters, digits and hyphens; starts with a letter, ends with a letter
/// or digit, no consecutive hyphens — the same rule as vault names.
fn is_valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (3..=24).contains(&bytes.len())
        && bytes.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-')
        && bytes[0].is_ascii_alphabetic()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && !name.contains("--")
}

const SETTABLE: [&str; 6] = [
    "enableSoftDelete",
    "softDeleteRetentionInDays",
    "enablePurgeProtection",
    "networkAcls",
    "publicNetworkAccess",
    "regions",
];

struct ManagedHsms;

impl ResourceTypeHandler for ManagedHsms {
    fn resource_type(&self) -> &'static str {
        HSM_TYPE
    }

    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        let name = request.name;
        if !is_valid_name(name) {
            return Err(invalid(format!(
                "The managed HSM name '{name}' is invalid. It must be 3 to 24 letters, digits and hyphens, start with a letter, end with a letter or digit, and not contain consecutive hyphens."
            )));
        }
        let given = request.body.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
        if let Some(existing) = request.existing {
            // Tenant and administrators are fixed; the rest can change.
            let mut properties = existing.properties.clone();
            for member in SETTABLE {
                if let Some(value) = given.get(member).filter(|v| !v.is_null()) {
                    properties[member] = value.clone();
                }
            }
            return Ok(properties);
        }

        let key = name.to_ascii_lowercase();
        let state = ctx.engine.state::<KeyVaultState>();
        let mut state = state.lock();
        let recover = given.get("createMode").and_then(Value::as_str) == Some("recover");
        if let Some(deleted) = state.deleted_hsms.get(&key) {
            if !recover {
                return Err(ArmError::conflict(
                    "ConflictError",
                    format!("A managed HSM with the name '{name}' already exists in deleted state. You need to either recover or purge it."),
                ));
            }
            let properties = deleted.resource.properties.clone();
            state.deleted_hsms.remove(&key);
            return Ok(properties);
        }
        if recover {
            return Err(ArmError::not_found(
                "DeletedManagedHsmNotFound",
                format!("There is no deleted managed HSM named '{name}' to recover."),
            ));
        }
        if state.hsm_names.contains(&key) {
            return Err(ArmError::conflict(
                "Conflict",
                format!("The managed HSM name '{name}' is already in use."),
            ));
        }
        let tenant = given
            .get("tenantId")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| invalid("The request content was invalid: the required property 'properties.tenantId' is missing."))?;
        let admins = given
            .get("initialAdminObjectIds")
            .and_then(Value::as_array)
            .filter(|ids| !ids.is_empty() && ids.iter().all(Value::is_string))
            .ok_or_else(|| invalid("The request content was invalid: 'properties.initialAdminObjectIds' must list at least one object id."))?;
        let retention = given.get("softDeleteRetentionInDays").and_then(Value::as_i64).unwrap_or(90);
        if !(7..=90).contains(&retention) {
            return Err(invalid("'properties.softDeleteRetentionInDays' must be between 7 and 90."));
        }
        state.hsm_names.insert(key);

        let mut properties = json!({
            "tenantId": tenant,
            "initialAdminObjectIds": admins,
            "hsmUri": ctx.engine.config().managed_hsm_uri(&name.to_ascii_lowercase()),
            "enableSoftDelete": true,
            "softDeleteRetentionInDays": retention,
            "enablePurgeProtection": false,
            "publicNetworkAccess": "Enabled",
            "networkAcls": {"bypass": "AzureServices", "defaultAction": "Allow", "ipRules": [], "virtualNetworkRules": []},
            "privateEndpointConnections": [],
            "regions": [],
            "provisioningState": "Succeeded",
            "statusMessage": "The Managed HSM is provisioned and ready to use.",
            "securityDomainProperties": {
                "activationStatus": "NotActivated",
                "activationStatusMessage": "Your HSM has been provisioned, but cannot be used for cryptographic operations until it is activated.",
            },
            "scheduledPurgeDate": null,
        });
        for member in SETTABLE {
            if let Some(value) = given.get(member).filter(|v| !v.is_null()) {
                properties[member] = value.clone();
            }
        }
        if let Some(members) = properties.as_object_mut() {
            members.remove("scheduledPurgeDate");
        }
        Ok(properties)
    }

    fn patch(&self, ctx: &ArmContext<'_>, existing: &Resource, body: &Value) -> Result<Value, ArmError> {
        self.put(
            ctx,
            &PutRequest {
                id: &existing.id,
                name: &existing.name,
                location: None,
                existing: Some(existing),
                body,
            },
        )
    }

    fn prepare(&self, _ctx: &ArmContext<'_>, resource: &mut Resource) {
        if resource.sku.is_none() {
            resource.sku = Some(json!({"family": "B", "name": "Standard_B1"}));
        }
    }

    fn deleted(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        let retention = resource.properties.get("softDeleteRetentionInDays").and_then(Value::as_i64).unwrap_or(90);
        let now = chrono::Utc::now();
        let render = |time: chrono::DateTime<chrono::Utc>| time.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        // The name stays taken while the HSM is soft-deleted.
        ctx.engine.state::<KeyVaultState>().lock().deleted_hsms.insert(
            resource.name.to_ascii_lowercase(),
            DeletedHsm {
                resource: resource.clone(),
                deletion_date: render(now),
                scheduled_purge_date: render(now + chrono::Duration::days(retention)),
            },
        );
    }
}

fn check_name_availability(ctx: &ArmContext<'_>, body: &Value) -> Result<Value, ArmError> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("The request content was invalid: the required property 'name' is missing."))?;
    if !is_valid_name(name) {
        return Ok(json!({"nameAvailable": false, "reason": "Invalid", "message": format!("The managed HSM name '{name}' is invalid.")}));
    }
    let taken = ctx.engine.state::<KeyVaultState>().lock().hsm_names.contains(&name.to_ascii_lowercase());
    Ok(if taken {
        json!({"nameAvailable": false, "reason": "AlreadyExists", "message": format!("The managed HSM name '{name}' is already in use.")})
    } else {
        json!({"nameAvailable": true})
    })
}

type Reply = Result<(StatusCode, Value), ArmError>;

fn deleted_json(ctx: &ArmContext<'_>, deleted: &DeletedHsm) -> Value {
    let hsm = &deleted.resource;
    let location = hsm.location.clone().unwrap_or_default();
    json!({
        "id": format!(
            "/subscriptions/{}/providers/Microsoft.KeyVault/locations/{location}/deletedManagedHSMs/{}",
            ctx.subscription_id, hsm.name
        ),
        "name": hsm.name,
        "type": "Microsoft.KeyVault/deletedManagedHSMs",
        "properties": {
            "mhsmId": hsm.id,
            "location": location,
            "deletionDate": deleted.deletion_date,
            "scheduledPurgeDate": deleted.scheduled_purge_date,
            "purgeProtectionEnabled": hsm.properties.get("enablePurgeProtection").and_then(Value::as_bool).unwrap_or(false),
            "tags": hsm.tags.clone().unwrap_or_else(|| json!({})),
        },
    })
}

fn deleted_not_found(name: &str) -> ArmError {
    ArmError::not_found("DeletedManagedHsmNotFound", format!("The deleted managed HSM '{name}' was not found."))
}

fn in_location(deleted: &DeletedHsm, location: &str) -> bool {
    let normalized: String = location.chars().filter(|c| !c.is_whitespace()).collect();
    deleted.resource.location.as_deref().is_some_and(|l| l.eq_ignore_ascii_case(&normalized))
}

fn list_deleted(ctx: &ArmContext<'_>, _request: &OperationRequest<'_>) -> Reply {
    let state = ctx.engine.state::<KeyVaultState>();
    let state = state.lock();
    let items: Vec<Value> = state.deleted_hsms.values().map(|d| deleted_json(ctx, d)).collect();
    Ok((StatusCode::OK, json!({"value": items})))
}

fn get_deleted(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let name = request.param("name");
    let state = ctx.engine.state::<KeyVaultState>();
    let state = state.lock();
    state
        .deleted_hsms
        .get(&name.to_ascii_lowercase())
        .filter(|d| in_location(d, request.param("location")))
        .map(|d| (StatusCode::OK, deleted_json(ctx, d)))
        .ok_or_else(|| deleted_not_found(name))
}

fn purge_deleted(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let name = request.param("name");
    let key = name.to_ascii_lowercase();
    let state = ctx.engine.state::<KeyVaultState>();
    let mut state = state.lock();
    if !state.deleted_hsms.get(&key).is_some_and(|d| in_location(d, request.param("location"))) {
        return Err(deleted_not_found(name));
    }
    state.deleted_hsms.remove(&key);
    state.hsm_names.remove(&key);
    // Purging an HSM destroys its keys.
    state.hsm_keys.remove(&key);
    // The specification makes this a long-running operation: 202 only.
    Ok((StatusCode::ACCEPTED, Value::Null))
}

pub(crate) fn find_hsm(ctx: &ArmContext<'_>, name: &str) -> Result<Resource, ArmError> {
    let id = format!(
        "/subscriptions/{}/resourceGroups/{}/providers/{HSM_TYPE}/{name}",
        ctx.subscription_id, ctx.resource_group
    );
    let state = ctx.engine.state::<ArmState>();
    let state = state.lock();
    state.resource(&id).cloned().ok_or_else(|| {
        ArmError::not_found(
            "ResourceNotFound",
            format!(
                "The Resource '{HSM_TYPE}/{name}' under resource group '{}' was not found. For more details please go to https://aka.ms/ARMResourceNotFoundFix",
                ctx.resource_group
            ),
        )
    })
}

/// The HSM's primary region (where it was created) followed by any extended
/// regions it was configured with.
fn regions(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let hsm = find_hsm(ctx, request.param("name"))?;
    let mut items = vec![json!({
        "name": hsm.location.clone().unwrap_or_default(),
        "isPrimary": true,
        "provisioningState": "Succeeded",
    })];
    for region in hsm.properties.get("regions").and_then(Value::as_array).into_iter().flatten() {
        let Some(name) = region.get("name").and_then(Value::as_str) else {
            continue;
        };
        if Some(name) != hsm.location.as_deref() {
            items.push(json!({"name": name, "isPrimary": false, "provisioningState": "Succeeded"}));
        }
    }
    Ok((StatusCode::OK, json!({"value": items})))
}

fn private_link_resources(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let hsm = find_hsm(ctx, request.param("name"))?;
    Ok((
        StatusCode::OK,
        json!({"value": [{
            "id": format!("{}/privateLinkResources/managedhsm", hsm.id),
            "name": "managedhsm",
            "type": "Microsoft.KeyVault/managedHSMs/privateLinkResources",
            "properties": {
                "groupId": "managedhsm",
                "requiredMembers": ["default"],
                "requiredZoneNames": ["privatelink.managedhsm.azure.net"],
            },
        }]}),
    ))
}
