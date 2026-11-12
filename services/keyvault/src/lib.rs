//! Azure Key Vault: the `Microsoft.KeyVault/vaults` resource type on Resource Manager
//! and the secrets data plane on `{vault}.vault.<suffix>`.
//!
//! Not implemented yet: keys, certificates, vault soft-delete, and data-plane
//! authorization (access policies and RBAC are stored but not enforced).

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
}

/// Mounts the Key Vault resource provider on Resource Manager.
pub fn register(registry: &mut ArmRegistry) {
    registry.register_type(Vaults);
    registry.register_provider_action(
        "Microsoft.KeyVault/checkNameAvailability",
        Arc::new(check_name_availability),
    );
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
        if request.existing.is_none() {
            if state.vaults.contains_key(&key) {
                return Err(ArmError::conflict("VaultAlreadyExists", taken_message(name)));
            }
            state.vaults.insert(key, Vault::default());
        }

        let mut properties = Value::Object(requested.cloned().unwrap_or_default());
        properties["vaultUri"] = json!(ctx.engine.config().vault_uri(&name.to_ascii_lowercase()));
        properties["provisioningState"] = json!("Succeeded");
        Ok(properties)
    }

    fn deleted(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        ctx.engine
            .state::<KeyVaultState>()
            .lock()
            .vaults
            .remove(&resource.name.to_ascii_lowercase());
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
    let taken = ctx
        .engine
        .state::<KeyVaultState>()
        .lock()
        .vaults
        .contains_key(&name.to_ascii_lowercase());
    Ok(if taken {
        json!({"nameAvailable": false, "reason": "AlreadyExists", "message": taken_message(name)})
    } else {
        json!({"nameAvailable": true})
    })
}
