//! Key Vault operations on Resource Manager beyond the vault resource itself:
//! deleted vaults, access-policy updates, and the management view of secrets.

use std::sync::Arc;

use emulator_core::arm::{ArmContext, ArmRegistry, ArmState, OperationRequest, Resource};
use emulator_core::ArmError;
use http::StatusCode;
use serde_json::{json, Value};

use crate::{DeletedVault, KeyVaultState};

type Reply = Result<(StatusCode, Value), ArmError>;

pub(crate) fn register(registry: &mut ArmRegistry) {
    registry.register_operation("Vaults_ListDeleted", Arc::new(list_deleted));
    registry.register_operation("Vaults_GetDeleted", Arc::new(get_deleted));
    registry.register_operation("Vaults_PurgeDeleted", Arc::new(purge_deleted));
    registry.register_operation("Vaults_UpdateAccessPolicy", Arc::new(update_access_policy));
    registry.register_operation("PrivateLinkResources_ListByVault", Arc::new(private_link_resources));
    registry.register_operation("Secrets_List", Arc::new(list_secrets));
    registry.register_operation("Secrets_Get", Arc::new(get_secret));
    registry.register_operation("Secrets_CreateOrUpdate", Arc::new(put_secret));
    registry.register_operation("Secrets_Update", Arc::new(patch_secret));
}

fn ok(body: Value) -> Reply {
    Ok((StatusCode::OK, body))
}

// ---- deleted vaults -----------------------------------------------------------

fn deleted_json(ctx: &ArmContext<'_>, deleted: &DeletedVault) -> Value {
    let vault = &deleted.resource;
    let location = vault.location.clone().unwrap_or_default();
    json!({
        "id": format!(
            "/subscriptions/{}/providers/Microsoft.KeyVault/locations/{location}/deletedVaults/{}",
            ctx.subscription_id, vault.name
        ),
        "name": vault.name,
        "type": "Microsoft.KeyVault/deletedVaults",
        "properties": {
            "vaultId": vault.id,
            "location": location,
            "deletionDate": deleted.deletion_date,
            "scheduledPurgeDate": deleted.scheduled_purge_date,
            "tags": vault.tags.clone().unwrap_or_else(|| json!({})),
            "purgeProtectionEnabled": vault.properties.get("enablePurgeProtection").and_then(Value::as_bool).unwrap_or(false),
        },
    })
}

fn deleted_not_found(name: &str) -> ArmError {
    ArmError::not_found("DeletedVaultNotFound", format!("The specified deleted vault '{name}' does not exist."))
}

/// Whether the deleted vault lived in `location` (`East US` and `eastus` match).
fn in_location(deleted: &DeletedVault, location: &str) -> bool {
    let normalized: String = location.chars().filter(|c| !c.is_whitespace()).collect();
    deleted.resource.location.as_deref().is_some_and(|l| l.eq_ignore_ascii_case(&normalized))
}

fn list_deleted(ctx: &ArmContext<'_>, _request: &OperationRequest<'_>) -> Reply {
    let state = ctx.engine.state::<KeyVaultState>();
    let state = state.lock();
    let items: Vec<Value> = state.deleted.values().map(|d| deleted_json(ctx, d)).collect();
    ok(json!({"value": items}))
}

fn get_deleted(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let name = request.param("vaultName");
    let state = ctx.engine.state::<KeyVaultState>();
    let state = state.lock();
    state
        .deleted
        .get(&name.to_ascii_lowercase())
        .filter(|deleted| in_location(deleted, request.param("location")))
        .map(|deleted| (StatusCode::OK, deleted_json(ctx, deleted)))
        .ok_or_else(|| deleted_not_found(name))
}

fn purge_deleted(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let name = request.param("vaultName");
    let key = name.to_ascii_lowercase();
    let state = ctx.engine.state::<KeyVaultState>();
    let mut state = state.lock();
    if !state.deleted.get(&key).is_some_and(|d| in_location(d, request.param("location"))) {
        return Err(deleted_not_found(name));
    }
    // The vault's contents are dropped with it, and the name becomes free.
    state.deleted.remove(&key);
    // A long-running operation: 202 with a status URL, which reports it done.
    Ok((StatusCode::ACCEPTED, Value::Null))
}

// ---- access policies ----------------------------------------------------------

pub(crate) fn vault_id(ctx: &ArmContext<'_>, name: &str) -> String {
    format!(
        "/subscriptions/{}/resourceGroups/{}/providers/Microsoft.KeyVault/vaults/{name}",
        ctx.subscription_id, ctx.resource_group
    )
}

pub(crate) fn vault_not_found(ctx: &ArmContext<'_>, name: &str) -> ArmError {
    ArmError::not_found(
        "ResourceNotFound",
        format!(
            "The Resource 'Microsoft.KeyVault/vaults/{name}' under resource group '{}' was not found. For more details please go to https://aka.ms/ARMResourceNotFoundFix",
            ctx.resource_group
        ),
    )
}

pub(crate) fn find_vault(ctx: &ArmContext<'_>, name: &str) -> Result<Resource, ArmError> {
    let state = ctx.engine.state::<ArmState>();
    let state = state.lock();
    state.resource(&vault_id(ctx, name)).cloned().ok_or_else(|| vault_not_found(ctx, name))
}

/// Who an access policy is for: tenant, object and (optionally) application.
fn principal(policy: &Value) -> [String; 3] {
    ["tenantId", "objectId", "applicationId"]
        .map(|member| policy.get(member).and_then(Value::as_str).unwrap_or("").to_ascii_lowercase())
}

const PERMISSION_KINDS: [&str; 4] = ["keys", "secrets", "certificates", "storage"];

fn permissions(policy: &Value, kind: &str) -> Vec<String> {
    policy
        .pointer(&format!("/permissions/{kind}"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn update_access_policy(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let name = request.param("vaultName");
    let kind = request.param("operationKind").to_ascii_lowercase();
    if !["add", "replace", "remove"].contains(&kind.as_str()) {
        return Err(ArmError::bad_request(
            "BadRequest",
            "The access policy operation kind must be add, replace or remove.",
        ));
    }
    let changes = request
        .body
        .pointer("/properties/accessPolicies")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ArmError::bad_request(
                "BadRequest",
                "The request content was invalid: the required property 'properties.accessPolicies' is missing.",
            )
        })?;

    let state = ctx.engine.state::<ArmState>();
    let mut state = state.lock();
    let vault = state.resource_mut(&vault_id(ctx, name)).ok_or_else(|| vault_not_found(ctx, name))?;
    let mut policies: Vec<Value> = vault
        .properties
        .get("accessPolicies")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for change in changes {
        let who = principal(change);
        let position = policies.iter().position(|existing| principal(existing) == who);
        match (kind.as_str(), position) {
            ("replace", Some(at)) => policies[at] = change.clone(),
            ("add" | "replace", None) => policies.push(change.clone()),
            ("add", Some(at)) => {
                // Adding to an existing policy grants the union of both.
                for permission_kind in PERMISSION_KINDS {
                    let mut merged = permissions(&policies[at], permission_kind);
                    for extra in permissions(change, permission_kind) {
                        if !merged.iter().any(|have| have.eq_ignore_ascii_case(&extra)) {
                            merged.push(extra);
                        }
                    }
                    if !merged.is_empty() {
                        policies[at]["permissions"][permission_kind] = json!(merged);
                    }
                }
            }
            ("remove", Some(at)) => {
                let names_permissions = PERMISSION_KINDS.iter().any(|k| !permissions(change, k).is_empty());
                if names_permissions {
                    for permission_kind in PERMISSION_KINDS {
                        let revoked = permissions(change, permission_kind);
                        let kept: Vec<String> = permissions(&policies[at], permission_kind)
                            .into_iter()
                            .filter(|have| !revoked.iter().any(|r| r.eq_ignore_ascii_case(have)))
                            .collect();
                        if policies[at].pointer(&format!("/permissions/{permission_kind}")).is_some() {
                            policies[at]["permissions"][permission_kind] = json!(kept);
                        }
                    }
                }
                // A policy with nothing left, or named without permissions, goes entirely.
                let empty = PERMISSION_KINDS.iter().all(|k| permissions(&policies[at], k).is_empty());
                if empty || !names_permissions {
                    policies.remove(at);
                }
            }
            _ => {}
        }
    }
    vault.properties["accessPolicies"] = json!(policies);
    ok(json!({
        "id": format!("{}/accessPolicies/", vault.id),
        "type": "Microsoft.KeyVault/vaults/accessPolicies",
        "properties": {"accessPolicies": policies},
    }))
}

fn private_link_resources(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let vault = find_vault(ctx, request.param("vaultName"))?;
    ok(json!({"value": [{
        "id": format!("{}/privateLinkResources/vault", vault.id),
        "name": "vault",
        "type": "Microsoft.KeyVault/vaults/privateLinkResources",
        "properties": {
            "groupId": "vault",
            "requiredMembers": ["default"],
            "requiredZoneNames": ["privatelink.vaultcore.azure.net"],
        },
    }]}))
}

// ---- secrets, as Resource Manager sees them -----------------------------------

fn secret_json(ctx: &ArmContext<'_>, vault: &Resource, summary: &crate::secrets::SecretSummary) -> Value {
    let uri = format!(
        "{}secrets/{}",
        ctx.engine.config().vault_uri(&vault.name.to_ascii_lowercase()),
        summary.name
    );
    let mut properties = json!({
        "attributes": summary.attributes,
        "secretUri": uri,
        "secretUriWithVersion": format!("{uri}/{}", summary.version),
    });
    if let Some(content_type) = &summary.content_type {
        properties["contentType"] = content_type.clone();
    }
    let mut out = json!({
        "id": format!("{}/secrets/{}", vault.id, summary.name),
        "name": summary.name,
        "type": "Microsoft.KeyVault/vaults/secrets",
        "location": vault.location,
        "properties": properties,
    });
    if let Some(tags) = &summary.tags {
        out["tags"] = tags.clone();
    }
    out
}

fn secret_not_found(vault: &Resource, name: &str) -> ArmError {
    ArmError::not_found(
        "SecretNotFound",
        format!("The secret '{name}' was not found in the key vault '{}'.", vault.name),
    )
}

/// The request's settings in the shape the secret store applies them in.
fn settings(body: &Value) -> Value {
    let mut out = json!({});
    for (member, pointer) in [
        ("contentType", "/properties/contentType"),
        ("attributes", "/properties/attributes"),
        ("tags", "/tags"),
    ] {
        if let Some(value) = body.pointer(pointer) {
            out[member] = value.clone();
        }
    }
    out
}

fn with_secrets<T>(
    ctx: &ArmContext<'_>,
    vault: &Resource,
    f: impl FnOnce(&mut crate::secrets::Vault) -> Result<T, ArmError>,
) -> Result<T, ArmError> {
    let state = ctx.engine.state::<KeyVaultState>();
    let mut state = state.lock();
    let data = state
        .vaults
        .get_mut(&vault.name.to_ascii_lowercase())
        .ok_or_else(|| vault_not_found(ctx, &vault.name))?;
    f(data)
}

fn list_secrets(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let vault = find_vault(ctx, request.param("vaultName"))?;
    let items = with_secrets(ctx, &vault, |data| Ok(data.summaries()))?;
    ok(json!({"value": items.iter().map(|s| secret_json(ctx, &vault, s)).collect::<Vec<_>>()}))
}

fn get_secret(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let vault = find_vault(ctx, request.param("vaultName"))?;
    let name = request.param("secretName");
    let summary = with_secrets(ctx, &vault, |data| data.summary(name).ok_or_else(|| secret_not_found(&vault, name)))?;
    ok(secret_json(ctx, &vault, &summary))
}

fn put_secret(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let vault = find_vault(ctx, request.param("vaultName"))?;
    let name = request.param("secretName");
    let valid_name = (1..=127).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if !valid_name {
        return Err(ArmError::bad_request(
            "BadRequest",
            format!("The secret name '{name}' is invalid. It must be 1 to 127 characters of letters, numbers and hyphens."),
        ));
    }
    let value = request.body.pointer("/properties/value").and_then(Value::as_str).ok_or_else(|| {
        ArmError::bad_request(
            "BadRequest",
            "The request content was invalid: the required property 'properties.value' is missing.",
        )
    })?;
    let (is_new, summary) = with_secrets(ctx, &vault, |data| {
        let is_new = data.add_version(name, value, &settings(request.body))?;
        Ok((is_new, data.summary(name).expect("just stored")))
    })?;
    let status = if is_new { StatusCode::CREATED } else { StatusCode::OK };
    Ok((status, secret_json(ctx, &vault, &summary)))
}

fn patch_secret(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let vault = find_vault(ctx, request.param("vaultName"))?;
    let name = request.param("secretName");
    let summary = with_secrets(ctx, &vault, |data| {
        if data.summary(name).is_none() {
            return Err(secret_not_found(&vault, name));
        }
        // A new value makes a new version; anything else edits the current one.
        match request.body.pointer("/properties/value").and_then(Value::as_str) {
            Some(value) => {
                data.add_version(name, value, &settings(request.body))?;
            }
            None => {
                data.update_current(name, &settings(request.body));
            }
        }
        Ok(data.summary(name).expect("exists"))
    })?;
    ok(secret_json(ctx, &vault, &summary))
}
