//! Keys through Resource Manager, in vaults and managed HSMs: create (only if the
//! key does not exist), get, list, and their versions.
//!
//! Resource Manager only ever shows a key's metadata: its type, size or curve,
//! permitted operations, attributes and URIs. Every version has real key
//! material, which the keys data plane (`keys_data`) uses.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use emulator_core::arm::{ArmContext, ArmRegistry, OperationRequest, Resource};
use emulator_core::ArmError;
use http::StatusCode;
use serde_json::{json, Value};

use crate::KeyVaultState;

/// Keys by lowercased name.
pub(crate) type KeyStore = BTreeMap<String, Key>;

#[derive(Clone)]
pub(crate) struct Key {
    /// The name as first created; ids keep this casing.
    pub(crate) name: String,
    /// Oldest first; the last one is the current version.
    pub(crate) versions: Vec<KeyVersion>,
}

#[derive(Clone)]
pub(crate) struct KeyVersion {
    pub(crate) version: String,
    pub(crate) kty: String,
    pub(crate) key_size: Option<i64>,
    pub(crate) curve: Option<String>,
    pub(crate) ops: Vec<String>,
    /// What the caller set: `enabled`, `nbf`, `exp`, `exportable`.
    pub(crate) attributes: serde_json::Map<String, Value>,
    pub(crate) tags: Option<Value>,
    pub(crate) rotation_policy: Option<Value>,
    pub(crate) release_policy: Option<Value>,
    pub(crate) created: i64,
    pub(crate) updated: i64,
    /// `None` only for symmetric (`oct-HSM`) keys in managed HSMs, which have no
    /// data plane here.
    pub(crate) material: Option<crate::crypto::Material>,
}

/// A soft-deleted key.
pub(crate) struct DeletedKey {
    pub(crate) key: Key,
    pub(crate) deleted_date: i64,
}

pub(crate) fn register(registry: &mut ArmRegistry) {
    for (prefix, holder) in [("Keys", Holder::Vault), ("ManagedHsmKeys", Holder::Hsm)] {
        registry.register_operation(id(prefix, "CreateIfNotExist"), Arc::new(move |ctx, request| create(ctx, request, holder)));
        registry.register_operation(id(prefix, "Get"), Arc::new(move |ctx, request| get(ctx, request, holder, false)));
        registry.register_operation(id(prefix, "GetVersion"), Arc::new(move |ctx, request| get(ctx, request, holder, true)));
        registry.register_operation(id(prefix, "List"), Arc::new(move |ctx, request| list(ctx, request, holder)));
        registry.register_operation(id(prefix, "ListVersions"), Arc::new(move |ctx, request| list_versions(ctx, request, holder)));
    }
}

fn id(prefix: &str, operation: &str) -> &'static str {
    // The registry keys operations by `'static` ids; these few are leaked once.
    Box::leak(format!("{prefix}_{operation}").into_boxed_str())
}

/// Where a key lives.
#[derive(Clone, Copy)]
enum Holder {
    Vault,
    Hsm,
}

type Reply = Result<(StatusCode, Value), ArmError>;

fn invalid(message: impl Into<String>) -> ArmError {
    ArmError::bad_request("BadRequest", message)
}

pub(crate) fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The holding resource (vault or HSM) the request names.
fn holder_resource(ctx: &ArmContext<'_>, request: &OperationRequest<'_>, holder: Holder) -> Result<Resource, ArmError> {
    match holder {
        Holder::Vault => crate::management::find_vault(ctx, request.param("vaultName")),
        Holder::Hsm => crate::managed_hsm::find_hsm(ctx, request.param("name")),
    }
}

/// Runs `f` on the holder's key store.
fn with_keys<T>(
    ctx: &ArmContext<'_>,
    resource: &Resource,
    holder: Holder,
    f: impl FnOnce(&mut KeyStore) -> Result<T, ArmError>,
) -> Result<T, ArmError> {
    let state = ctx.engine.state::<KeyVaultState>();
    let mut state = state.lock();
    let name = resource.name.to_ascii_lowercase();
    let store = match holder {
        Holder::Vault => &mut state
            .vaults
            .get_mut(&name)
            .ok_or_else(|| crate::management::vault_not_found(ctx, &resource.name))?
            .keys,
        Holder::Hsm => state.hsm_keys.entry(name).or_default(),
    };
    f(store)
}

fn base_uri(ctx: &ArmContext<'_>, resource: &Resource, holder: Holder) -> String {
    let name = resource.name.to_ascii_lowercase();
    match holder {
        Holder::Vault => ctx.engine.config().vault_uri(&name),
        Holder::Hsm => ctx.engine.config().managed_hsm_uri(&name),
    }
}

fn key_json(ctx: &ArmContext<'_>, resource: &Resource, holder: Holder, key: &Key, version: &KeyVersion, versioned: bool) -> Value {
    let uri = format!("{}keys/{}", base_uri(ctx, resource, holder), key.name);
    let mut attributes = Value::Object(version.attributes.clone());
    attributes["enabled"] = version.attributes.get("enabled").cloned().unwrap_or(json!(true));
    attributes["created"] = json!(version.created);
    attributes["updated"] = json!(version.updated);
    // Vaults and HSMs are always soft-deleted here, and purge is not blocked.
    attributes["recoveryLevel"] = json!("Recoverable+Purgeable");
    let mut properties = json!({
        "attributes": attributes,
        "kty": version.kty,
        "keyOps": version.ops,
        "keyUri": uri,
        "keyUriWithVersion": format!("{uri}/{}", version.version),
    });
    if let Some(size) = version.key_size {
        properties["keySize"] = json!(size);
    }
    if let Some(curve) = &version.curve {
        properties["curveName"] = json!(curve);
    }
    if let Some(policy) = &version.rotation_policy {
        properties["rotationPolicy"] = policy.clone();
    }
    if let Some(policy) = &version.release_policy {
        properties["release_policy"] = policy.clone();
    }
    let kind = match holder {
        Holder::Vault => "Microsoft.KeyVault/vaults/keys",
        Holder::Hsm => "Microsoft.KeyVault/managedHSMs/keys",
    };
    let mut out = json!({
        "id": format!("{}/keys/{}", resource.id, key.name),
        "name": key.name,
        "type": kind,
        "properties": properties,
    });
    if versioned {
        out["id"] = json!(format!("{}/keys/{}/versions/{}", resource.id, key.name, version.version));
        out["name"] = json!(version.version);
        out["type"] = json!(format!("{kind}/versions"));
    }
    if let (Holder::Vault, Some(location)) = (holder, &resource.location) {
        out["location"] = json!(location);
    }
    if let Some(tags) = &version.tags {
        out["tags"] = tags.clone();
    }
    out
}

fn key_not_found(name: &str) -> ArmError {
    ArmError::not_found("KeyNotFound", format!("The key '{name}' was not found."))
}

/// The operations a key of this type can be used for.
pub(crate) fn applicable_ops(kty: &str) -> &'static [&'static str] {
    match kty.trim_end_matches("-HSM") {
        "RSA" => &["encrypt", "decrypt", "sign", "verify", "wrapKey", "unwrapKey"],
        "EC" => &["sign", "verify"],
        _ => &["encrypt", "decrypt", "wrapKey", "unwrapKey"],
    }
}

fn create(ctx: &ArmContext<'_>, request: &OperationRequest<'_>, holder: Holder) -> Reply {
    let resource = holder_resource(ctx, request, holder)?;
    let name = request.param("keyName");
    if !(1..=127).contains(&name.len()) || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(invalid(format!("The key name '{name}' is invalid. It must be 1 to 127 letters, digits and hyphens.")));
    }
    let given = request.body.get("properties").cloned().unwrap_or_else(|| json!({}));
    let text = |member: &str| given.get(member).and_then(Value::as_str);

    // Creating a key that exists returns it unchanged: no new version.
    let existing = with_keys(ctx, &resource, holder, |store| {
        Ok(store.get(&name.to_ascii_lowercase()).map(|key| {
            key_json(ctx, &resource, holder, key, key.versions.last().expect("a key has a version"), false)
        }))
    })?;
    if let Some(body) = existing {
        return Ok((StatusCode::OK, body));
    }

    let kty = text("kty").ok_or_else(|| invalid("The request content was invalid: 'properties.kty' is required."))?.to_string();
    let allowed: &[&str] = match holder {
        Holder::Vault => &["RSA", "RSA-HSM", "EC", "EC-HSM"],
        Holder::Hsm => &["RSA", "RSA-HSM", "EC", "EC-HSM", "oct-HSM"],
    };
    if !allowed.contains(&kty.as_str()) {
        return Err(invalid(format!("The key type '{kty}' is not supported here. Supported types: {}.", allowed.join(", "))));
    }
    if matches!(holder, Holder::Vault) && kty.ends_with("-HSM") {
        let premium = resource.properties.pointer("/sku/name").and_then(Value::as_str).is_some_and(|s| s.eq_ignore_ascii_case("premium"));
        if !premium {
            return Err(invalid("HSM-protected keys require a Premium vault."));
        }
    }
    let size = given.get("keySize").and_then(Value::as_i64);
    let curve = text("curveName").map(str::to_string);
    let (key_size, curve) = match kty.trim_end_matches("-HSM") {
        "RSA" => {
            if curve.is_some() {
                return Err(invalid("An RSA key has no curve."));
            }
            let size = size.unwrap_or(2048);
            if ![2048, 3072, 4096].contains(&size) {
                return Err(invalid("An RSA key's size must be 2048, 3072 or 4096."));
            }
            (Some(size), None)
        }
        "EC" => {
            let curve = curve.unwrap_or_else(|| "P-256".into());
            let bits = match curve.as_str() {
                "P-256" | "P-256K" => 256,
                "P-384" => 384,
                "P-521" => 521,
                other => return Err(invalid(format!("The curve '{other}' is not supported."))),
            };
            if size.is_some_and(|s| s != bits) {
                return Err(invalid("An EC key's size, when given, must match its curve."));
            }
            (None, Some(curve))
        }
        _ => {
            let size = size.unwrap_or(256);
            if ![128, 192, 256].contains(&size) {
                return Err(invalid("A symmetric key's size must be 128, 192 or 256."));
            }
            (Some(size), None)
        }
    };
    let applicable = applicable_ops(&kty);
    let ops: Vec<String> = match given.get("keyOps").and_then(Value::as_array) {
        Some(requested) => {
            let mut ops = Vec::new();
            for op in requested {
                let op = op.as_str().unwrap_or("");
                if !applicable.contains(&op) && op != "import" {
                    return Err(invalid(format!("The operation '{op}' does not apply to a {kty} key.")));
                }
                ops.push(op.to_string());
            }
            ops
        }
        None => applicable.iter().map(|op| op.to_string()).collect(),
    };
    let mut attributes = serde_json::Map::new();
    for member in ["enabled", "nbf", "exp", "exportable"] {
        if let Some(value) = given.pointer(&format!("/attributes/{member}")).filter(|v| !v.is_null()) {
            attributes.insert(member.into(), value.clone());
        }
    }
    let material = match kty.as_str() {
        "oct-HSM" => None,
        _ => Some(crate::crypto::Material::generate(&kty, key_size, curve.as_deref(), None).map_err(invalid)?),
    };
    let created = now();
    let version = KeyVersion {
        version: uuid::Uuid::new_v4().simple().to_string(),
        kty,
        key_size,
        curve,
        ops,
        material,
        updated: created,
        attributes,
        tags: request.body.get("tags").filter(|t| !t.is_null()).cloned(),
        rotation_policy: given.get("rotationPolicy").filter(|v| !v.is_null()).cloned(),
        release_policy: given.get("release_policy").filter(|v| !v.is_null()).cloned(),
        created,
    };
    let body = with_keys(ctx, &resource, holder, |store| {
        let key = store.entry(name.to_ascii_lowercase()).or_insert_with(|| Key { name: name.to_string(), versions: Vec::new() });
        key.versions.push(version);
        Ok(key_json(ctx, &resource, holder, key, key.versions.last().expect("just pushed"), false))
    })?;
    Ok((StatusCode::OK, body))
}

fn get(ctx: &ArmContext<'_>, request: &OperationRequest<'_>, holder: Holder, versioned: bool) -> Reply {
    let resource = holder_resource(ctx, request, holder)?;
    let name = request.param("keyName");
    let body = with_keys(ctx, &resource, holder, |store| {
        let key = store.get(&name.to_ascii_lowercase()).ok_or_else(|| key_not_found(name))?;
        let version = if versioned {
            let wanted = request.param("keyVersion");
            key.versions.iter().find(|v| v.version == wanted).ok_or_else(|| key_not_found(&format!("{name}/{wanted}")))?
        } else {
            key.versions.last().expect("a key has a version")
        };
        Ok(key_json(ctx, &resource, holder, key, version, versioned))
    })?;
    Ok((StatusCode::OK, body))
}

fn list(ctx: &ArmContext<'_>, request: &OperationRequest<'_>, holder: Holder) -> Reply {
    let resource = holder_resource(ctx, request, holder)?;
    let items = with_keys(ctx, &resource, holder, |store| {
        Ok(store
            .values()
            .map(|key| key_json(ctx, &resource, holder, key, key.versions.last().expect("a key has a version"), false))
            .collect::<Vec<_>>())
    })?;
    Ok((StatusCode::OK, json!({"value": items})))
}

fn list_versions(ctx: &ArmContext<'_>, request: &OperationRequest<'_>, holder: Holder) -> Reply {
    let resource = holder_resource(ctx, request, holder)?;
    let name = request.param("keyName");
    let items = with_keys(ctx, &resource, holder, |store| {
        let key = store.get(&name.to_ascii_lowercase()).ok_or_else(|| key_not_found(name))?;
        Ok(key.versions.iter().map(|v| key_json(ctx, &resource, holder, key, v, true)).collect::<Vec<_>>())
    })?;
    Ok((StatusCode::OK, json!({"value": items})))
}
