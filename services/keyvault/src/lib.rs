//! Azure Key Vault: the `Microsoft.KeyVault/vaults` resource type on Resource Manager
//! and the secrets data plane on `{vault}.vault.<suffix>`.
//!
//! Vaults are soft-deleted: a deleted vault keeps its name and contents until it is
//! recovered (`createMode: recover`) or purged. Secrets are reachable through both
//! planes and share one store.
//!
//! Managed HSMs are modeled on Resource Manager (with soft delete), but their data
//! plane and security-domain activation are not.
//!
//! Keys are created and read through Resource Manager, in vaults and managed
//! HSMs, and used through the vault's keys data plane, with real key material.
//!
//! Not implemented yet: certificates, and the managed HSM data plane. Purge
//! protection is stored but does not block a purge.

mod cert_crypto;
mod certificates;
mod crypto;
mod keys;
mod keys_data;
mod managed_hsm;
mod management;
pub mod scaffold;
pub mod secrets;

use std::collections::BTreeMap;
use std::sync::Arc;

use emulator_core::arm::{ArmContext, ArmRegistry, PutRequest, Resource, ResourceTypeHandler};
use emulator_core::ArmError;
use serde_json::{json, Value};

use secrets::Vault;

const RESOURCE_TYPE: &str = "Microsoft.KeyVault/vaults";

/// Every vault's data, keyed by lowercased vault name (vault names are global).
#[derive(Default)]
pub struct KeyVaultState {
    pub(crate) vaults: BTreeMap<String, Vault>,
    /// Soft-deleted vaults, keyed the same way. Their names stay taken.
    pub(crate) deleted: BTreeMap<String, DeletedVault>,
    /// Managed HSM names in use, live or soft-deleted (lowercased); they are
    /// global, separately from vault names.
    pub(crate) hsm_names: std::collections::BTreeSet<String>,
    /// Soft-deleted managed HSMs, keyed by lowercased name.
    pub(crate) deleted_hsms: BTreeMap<String, DeletedHsm>,
    /// Each managed HSM's keys, by lowercased HSM name. They survive a soft
    /// delete and go with a purge.
    pub(crate) hsm_keys: BTreeMap<String, keys::KeyStore>,
}

/// A managed HSM between deletion and recovery or purge.
pub(crate) struct DeletedHsm {
    pub resource: Resource,
    pub deletion_date: String,
    pub scheduled_purge_date: String,
}

/// A vault between deletion and recovery or purge.
pub(crate) struct DeletedVault {
    /// The vault resource as it was when deleted.
    pub resource: Resource,
    pub deletion_date: String,
    pub scheduled_purge_date: String,
    pub data: Vault,
}

/// Mounts the Key Vault resource provider on Resource Manager.
pub fn register(registry: &mut ArmRegistry) {
    registry.register_type(Vaults);
    registry.register_provider_action(
        "Microsoft.KeyVault/checkNameAvailability",
        Arc::new(check_name_availability),
    );
    management::register(registry);
    managed_hsm::register(registry);
    keys::register(registry);
    for kind in [
        "Microsoft.KeyVault/vaults/privateEndpointConnections",
        "Microsoft.KeyVault/managedHSMs/privateEndpointConnections",
        "Microsoft.KeyVault/vaults/networkSecurityPerimeterConfigurations",
    ] {
        registry.register_type(emulator_core::arm::CreatedByNetwork(kind));
    }
    // Everything else in the specification is mounted as a 501 stub.
    registry.register_scaffold(&scaffold::PROVIDER);
}

fn is_valid_vault_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (3..=24).contains(&bytes.len())
        && bytes.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-')
        && bytes[0].is_ascii_alphabetic()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && !name.contains("--")
}

fn invalid_name_message(name: &str) -> String {
    format!("The vault name '{name}' is invalid. A vault's name must be between 3-24 alphanumeric characters. The name must begin with a letter, end with a letter or digit, and not contain consecutive hyphens. Follow this link for more information: https://go.microsoft.com/fwlink/?linkid=2147742")
}

fn taken_message(name: &str) -> String {
    format!("The vault name '{name}' is already in use. Vault names are globally unique so it is possible that the name is already taken.")
}

struct Vaults;

impl ResourceTypeHandler for Vaults {
    fn resource_type(&self) -> &'static str {
        RESOURCE_TYPE
    }

    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        let name = request.name;
        if !is_valid_vault_name(name) {
            return Err(ArmError::bad_request("VaultNameNotValid", invalid_name_message(name)));
        }
        let requested = request.body.get("properties").and_then(Value::as_object);
        // The wording of this refusal has not been checked against Azure.
        for member in ["tenantId", "sku"] {
            if requested.and_then(|p| p.get(member)).is_none_or(Value::is_null) {
                return Err(ArmError::bad_request(
                    "BadRequest",
                    format!("The request content was invalid: the required property 'properties.{member}' is missing."),
                ));
            }
        }

        let state = ctx.engine.state::<KeyVaultState>();
        let mut state = state.lock();
        let key = name.to_ascii_lowercase();
        let recover = requested.and_then(|p| p.get("createMode")).and_then(Value::as_str) == Some("recover");
        let mut recovered = None;
        if request.existing.is_none() {
            if state.vaults.contains_key(&key) {
                return Err(ArmError::conflict("VaultAlreadyExists", taken_message(name)));
            }
            match (state.deleted.contains_key(&key), recover) {
                (true, true) => {
                    // Recovery brings back the vault's settings and its contents.
                    let deleted = state.deleted.remove(&key).expect("checked above");
                    state.vaults.insert(key, deleted.data);
                    recovered = Some(deleted.resource.properties);
                }
                (true, false) => {
                    return Err(ArmError::conflict(
                        "ConflictError",
                        format!("A vault with the same name already exists in deleted state. You need to either recover or purge existing key vault. Follow this link https://go.microsoft.com/fwlink/?linkid=2149745 for more information on soft delete. (vault: {name})"),
                    ));
                }
                (false, _) => {
                    state.vaults.insert(key, Vault::default());
                }
            }
        }

        let mut properties = match recovered {
            Some(properties) => properties,
            None => Value::Object(requested.cloned().unwrap_or_default()),
        };
        // `createMode` is an instruction, not a setting: it is never stored.
        if let Some(members) = properties.as_object_mut() {
            members.remove("createMode");
        }
        properties["vaultUri"] = json!(ctx.engine.config().vault_uri(&name.to_ascii_lowercase()));
        properties["provisioningState"] = json!("Succeeded");
        Ok(properties)
    }

    fn deleted(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        let key = resource.name.to_ascii_lowercase();
        let state = ctx.engine.state::<KeyVaultState>();
        let mut state = state.lock();
        let Some(data) = state.vaults.remove(&key) else {
            return;
        };
        let retention_days = resource
            .properties
            .get("softDeleteRetentionInDays")
            .and_then(Value::as_i64)
            .unwrap_or(90);
        let now = chrono::Utc::now();
        let render = |time: chrono::DateTime<chrono::Utc>| time.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        state.deleted.insert(
            key,
            DeletedVault {
                resource: resource.clone(),
                deletion_date: render(now),
                scheduled_purge_date: render(now + chrono::Duration::days(retention_days)),
                data,
            },
        );
    }
}

fn check_name_availability(ctx: &ArmContext<'_>, body: &Value) -> Result<Value, ArmError> {
    let name = body.get("name").and_then(Value::as_str).ok_or_else(|| {
        ArmError::bad_request(
            "BadRequest",
            "The request content was invalid: the required property 'name' is missing.",
        )
    })?;
    if !is_valid_vault_name(name) {
        return Ok(json!({
            "nameAvailable": false,
            "reason": "Invalid",
            "message": invalid_name_message(name),
        }));
    }
    let taken = {
        let state = ctx.engine.state::<KeyVaultState>();
        let state = state.lock();
        let key = name.to_ascii_lowercase();
        // A soft-deleted vault still holds its name.
        state.vaults.contains_key(&key) || state.deleted.contains_key(&key)
    };
    Ok(if taken {
        json!({"nameAvailable": false, "reason": "AlreadyExists", "message": taken_message(name)})
    } else {
        json!({"nameAvailable": true})
    })
}
