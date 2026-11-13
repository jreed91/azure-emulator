//! Azure Storage: the `Microsoft.Storage/storageAccounts` resource type on Resource
//! Manager and the Blob service data plane on `{account}.blob.core.<suffix>`.

pub mod blob;
mod management;
mod sas;
pub mod scaffold;
mod shared_key;
mod state;

use std::sync::Arc;

use base64::Engine as _;
use emulator_core::arm::{ArmContext, ArmRegistry, PutRequest, Resource, ResourceTypeHandler};
use emulator_core::ArmError;
use rand::RngCore;
use serde_json::{json, Value};

pub use state::StorageState;
use state::Account;

const RESOURCE_TYPE: &str = "Microsoft.Storage/storageAccounts";

/// Mounts the Storage resource provider on Resource Manager.
pub fn register(registry: &mut ArmRegistry) {
    registry.register_type(StorageAccounts);
    registry.register_provider_action(
        "Microsoft.Storage/checkNameAvailability",
        Arc::new(check_name_availability),
    );
    management::register(registry);
    // Everything else in the specification is mounted as a 501 stub.
    registry.register_scaffold(&scaffold::PROVIDER);
}

fn now_iso() -> String {
    emulator_core::http_util::iso_timestamp()
}

/// A new 512-bit account key, base64 encoded as the service returns it.
pub(crate) fn new_key() -> String {
    let mut bytes = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn is_valid_account_name(name: &str) -> bool {
    (3..=24).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

fn invalid_name_message(name: &str) -> String {
    format!("{name} is not a valid storage account name. Storage account name must be between 3 and 24 characters in length and use numbers and lower-case letters only.")
}

fn taken_message(name: &str) -> String {
    format!("The storage account named {name} is already taken.")
}

fn keys_json(account: &Account) -> Value {
    json!({"keys": [
        {"creationTime": account.key_created[0], "keyName": "key1", "value": account.keys[0], "permissions": "FULL"},
        {"creationTime": account.key_created[1], "keyName": "key2", "value": account.keys[1], "permissions": "FULL"},
    ]})
}

struct StorageAccounts;

impl ResourceTypeHandler for StorageAccounts {
    fn resource_type(&self) -> &'static str {
        RESOURCE_TYPE
    }

    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        let name = request.name;
        if !is_valid_account_name(name) {
            return Err(ArmError::bad_request("AccountNameInvalid", invalid_name_message(name)));
        }
        let body = request.body;
        if request.existing.is_none() {
            // The wording of these two refusals has not been checked against Azure.
            for (member, present) in [
                ("sku", body.pointer("/sku/name").and_then(Value::as_str).is_some()),
                ("kind", body.get("kind").and_then(Value::as_str).is_some()),
            ] {
                if !present {
                    return Err(ArmError::bad_request(
                        "InvalidRequestContent",
                        format!("The request content was invalid: the required property '{member}' is missing."),
                    ));
                }
            }
        }

        let state = ctx.engine.state::<StorageState>();
        let mut state = state.lock();
        if request.existing.is_none() {
            // Account names are global: they are DNS labels.
            if state.accounts.contains_key(name) {
                return Err(ArmError::conflict("StorageAccountAlreadyTaken", taken_message(name)));
            }
            let created = now_iso();
            state.accounts.insert(
                name.to_string(),
                Account {
                    keys: [new_key(), new_key()],
                    key_created: [created.clone(), created],
                    containers: Default::default(),
                },
            );
        }

        // Caller-settable properties pass through; the service-owned ones overlay them.
        let mut properties = request
            .existing
            .map(|r| r.properties.clone())
            .unwrap_or_else(|| json!({}));
        if let Some(requested) = body.get("properties").and_then(Value::as_object) {
            for (key, value) in requested {
                properties[key] = value.clone();
            }
        }
        if properties.get("supportsHttpsTrafficOnly").is_none() {
            properties["supportsHttpsTrafficOnly"] = json!(true);
        }
        let kind = body
            .get("kind")
            .and_then(Value::as_str)
            .or_else(|| request.existing.and_then(|r| r.kind.as_ref()).and_then(Value::as_str))
            .unwrap_or("");
        if properties.get("accessTier").is_none() && matches!(kind, "StorageV2" | "BlobStorage") {
            properties["accessTier"] = json!("Hot");
        }
        if properties.get("creationTime").is_none() {
            properties["creationTime"] = json!(now_iso());
        }
        properties["provisioningState"] = json!("Succeeded");
        properties["primaryLocation"] = json!(request.location);
        properties["statusOfPrimary"] = json!("available");
        // Only the services the emulator serves are advertised.
        properties["primaryEndpoints"] = json!({"blob": ctx.engine.config().blob_endpoint(name)});
        Ok(properties)
    }

    fn actions(&self) -> &'static [&'static str] {
        &["listKeys", "regenerateKey", "listAccountSas", "listServiceSas"]
    }

    fn created(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        management::create_services(ctx, resource);
    }

    fn deleted(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        ctx.engine.state::<StorageState>().lock().accounts.remove(&resource.name);
    }

    fn action(
        &self,
        ctx: &ArmContext<'_>,
        resource: &Resource,
        action: &str,
        body: &Value,
    ) -> Result<Value, ArmError> {
        let state = ctx.engine.state::<StorageState>();
        let mut state = state.lock();
        let account = state.accounts.get_mut(&resource.name).ok_or_else(|| {
            ArmError::not_found(
                "ResourceNotFound",
                format!("The storage account '{}' was not found.", resource.name),
            )
        })?;
        match action.to_ascii_lowercase().as_str() {
            "listkeys" => Ok(keys_json(account)),
            "regeneratekey" => {
                let index = match body.get("keyName").and_then(Value::as_str) {
                    Some("key1") => 0,
                    Some("key2") => 1,
                    _ => {
                        return Err(ArmError::bad_request(
                            "InvalidValuesForRequestParameters",
                            "Values for request parameters are invalid: keyName.",
                        ))
                    }
                };
                account.keys[index] = new_key();
                account.key_created[index] = now_iso();
                Ok(keys_json(account))
            }
            "listaccountsas" => {
                let token = sas_token(&resource.name, account, body, true)?;
                Ok(json!({"accountSasToken": token}))
            }
            "listservicesas" => {
                let token = sas_token(&resource.name, account, body, false)?;
                Ok(json!({"serviceSasToken": token}))
            }
            other => Err(ArmError::not_implemented(format_args!(
                "the '{other}' action on {RESOURCE_TYPE}"
            ))),
        }
    }
}

/// The service version the tokens Resource Manager issues are signed for.
const ISSUED_SAS_VERSION: &str = "2015-04-05";

/// Issues an account SAS (`for_account`) or a service SAS from the request's
/// `signed*` members, signed with the account key the request names.
fn sas_token(account_name: &str, account: &Account, body: &Value, for_account: bool) -> Result<String, ArmError> {
    let text = |member: &str| body.get(member).and_then(Value::as_str).unwrap_or("").to_string();
    let missing = |member: &str| {
        ArmError::bad_request(
            "InvalidValuesForRequestParameters",
            format!("Values for request parameters are invalid: {member}."),
        )
    };
    // Token parameter name, then the request member it comes from.
    let members: &[(&str, &str)] = if for_account {
        &[
            ("ss", "signedServices"),
            ("srt", "signedResourceTypes"),
            ("sp", "signedPermission"),
            ("st", "signedStart"),
            ("se", "signedExpiryTime"),
            ("sip", "signedIp"),
            ("spr", "signedProtocol"),
        ]
    } else {
        &[
            ("sr", "signedResource"),
            ("sp", "signedPermission"),
            ("st", "signedStart"),
            ("se", "signedExpiryTime"),
            ("sip", "signedIp"),
            ("spr", "signedProtocol"),
            ("rscc", "rscc"),
            ("rscd", "rscd"),
            ("rsce", "rsce"),
            ("rscl", "rscl"),
            ("rsct", "rsct"),
        ]
    };
    let required: &[&str] = if for_account {
        &["signedServices", "signedResourceTypes", "signedPermission", "signedExpiryTime"]
    } else {
        &["canonicalizedResource", "signedResource", "signedPermission", "signedExpiryTime"]
    };
    for member in required {
        if text(member).is_empty() {
            return Err(missing(member));
        }
    }
    if !text("signedIdentifier").is_empty() {
        return Err(ArmError::not_implemented("shared access signatures that name a stored access policy"));
    }
    for member in ["signedExpiryTime", "signedStart"] {
        if !text(member).is_empty() && sas::parse_time(&text(member)).is_none() {
            return Err(missing(member));
        }
    }
    let key = match text("keyToSign").as_str() {
        "" | "key1" => &account.keys[0],
        "key2" => &account.keys[1],
        _ => return Err(missing("keyToSign")),
    };

    let mut fields: std::collections::BTreeMap<String, String> = members
        .iter()
        .map(|(parameter, member)| (parameter.to_string(), text(member)))
        .filter(|(_, value)| !value.is_empty())
        .collect();
    fields.insert("sv".into(), ISSUED_SAS_VERSION.into());
    let string_to_sign = if for_account {
        sas::account_string_to_sign(account_name, &sas::Fields(&fields))
    } else {
        sas::service_string_to_sign(&text("canonicalizedResource"), &sas::Fields(&fields))
    };
    let signature = sas::sign(key, &string_to_sign)
        .ok_or_else(|| ArmError::new(http::StatusCode::INTERNAL_SERVER_ERROR, "InternalError", "The account key is not valid base64."))?;

    let encode = |value: &str| {
        const KEEP: percent_encoding::AsciiSet = percent_encoding::NON_ALPHANUMERIC
            .remove(b'-')
            .remove(b'.')
            .remove(b'_')
            .remove(b'~');
        percent_encoding::utf8_percent_encode(value, &KEEP).to_string()
    };
    let mut token = format!("sv={ISSUED_SAS_VERSION}");
    for (parameter, _) in members {
        if let Some(value) = fields.get(*parameter) {
            token.push_str(&format!("&{parameter}={}", encode(value)));
        }
    }
    token.push_str(&format!("&sig={}", encode(&signature)));
    Ok(token)
}

fn check_name_availability(ctx: &ArmContext<'_>, body: &Value) -> Result<Value, ArmError> {
    let name = body.get("name").and_then(Value::as_str).ok_or_else(|| {
        ArmError::bad_request(
            "InvalidRequestContent",
            "The request content was invalid: the required property 'name' is missing.",
        )
    })?;
    if !is_valid_account_name(name) {
        return Ok(json!({
            "nameAvailable": false,
            "reason": "AccountNameInvalid",
            "message": invalid_name_message(name),
        }));
    }
    let taken = ctx.engine.state::<StorageState>().lock().accounts.contains_key(name);
    Ok(if taken {
        json!({"nameAvailable": false, "reason": "AlreadyExists", "message": taken_message(name)})
    } else {
        json!({"nameAvailable": true})
    })
}
