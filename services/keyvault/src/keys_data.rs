//! The Key Vault keys data plane, on `{vault}.vault.<suffix>/keys`.
//!
//! Implemented: create (RSA and EC, including `-HSM` types in Premium vaults),
//! import, get (latest or by version), update, list, list versions, rotate,
//! soft delete with get/list deleted, recover and purge, and the cryptographic
//! operations: encrypt, decrypt, sign, verify, wrapKey and unwrapKey. Keys are
//! the same store Resource Manager's keys API reads.

use serde_json::{json, Map, Value};

use http::{Method, StatusCode};

use crate::crypto::{self, Material};
use crate::keys::{applicable_ops, now, DeletedKey, Key, KeyVersion};
use crate::secrets::Vault;
use crate::KeyVaultState;
use emulator_core::{ArmError, Engine};

/// Days a deleted key stays recoverable.
const RECOVERABLE_DAYS: i64 = 90;

fn bad(message: impl Into<String>) -> ArmError {
    ArmError::bad_request("BadParameter", message)
}

fn key_not_found(name: &str) -> ArmError {
    ArmError::not_found(
        "KeyNotFound",
        format!("A key with (name/id) {name} was not found in this key vault. If you recently deleted this key you may be able to recover it using the correct recovery command. For help resolving this issue, please see https://go.microsoft.com/fwlink/?linkid=2125182"),
    )
}

fn deleted_not_found(name: &str) -> ArmError {
    ArmError::not_found("KeyNotFound", format!("Deleted Key not found: {name}"))
}

/// The operations and the crypto operation names their paths use.
const CRYPTO: &[(&str, &str)] = &[
    ("encrypt", "encrypt"),
    ("decrypt", "decrypt"),
    ("sign", "sign"),
    ("verify", "verify"),
    ("wrapkey", "wrapKey"),
    ("unwrapkey", "unwrapKey"),
];

struct Keys<'a> {
    engine: &'a Engine,
    vault: &'a str,
    body: &'a Value,
}

/// Serves a request under `/keys`, `/deletedkeys` or `/rng`. `None` is an
/// empty `204` answer.
pub(crate) fn serve(
    engine: &Engine,
    vault: &str,
    method: &Method,
    collection: &str,
    rest: &[&str],
    body: &Value,
) -> Result<Option<Value>, ArmError> {
    let keys = Keys { engine, vault, body };
    let lower: Vec<String> = rest.iter().map(|s| s.to_ascii_lowercase()).collect();
    let lower: Vec<&str> = lower.iter().map(String::as_str).collect();
    let crypto_op = |segment: &str| CRYPTO.iter().find(|(path, _)| *path == segment).map(|(_, op)| *op);
    match (collection, method.as_str(), lower.as_slice()) {
        ("keys", "GET", []) => keys.list().map(Some),
        ("keys", "POST", [_, "create"]) => keys.create(rest[0]).map(Some),
        ("keys", "PUT", [_]) => keys.import(rest[0]).map(Some),
        ("keys", "POST", [_, "rotate"]) => keys.rotate(rest[0]).map(Some),
        ("keys", "GET", [_, "versions"]) => keys.versions(rest[0]).map(Some),
        ("keys", "GET", [_, "rotationpolicy"]) => keys.rotation_policy(rest[0], None).map(Some),
        ("keys", "PUT", [_, "rotationpolicy"]) => keys.rotation_policy(rest[0], Some(body)).map(Some),
        ("keys", "POST", [_, op]) if crypto_op(op).is_some() => keys.crypto(rest[0], None, crypto_op(op).expect("checked")).map(Some),
        ("keys", "POST", [_, _, op]) if crypto_op(op).is_some() => {
            keys.crypto(rest[0], Some(rest[1]), crypto_op(op).expect("checked")).map(Some)
        }
        ("keys", "GET", [_]) => keys.get(rest[0], None).map(Some),
        ("keys", "GET", [_, _]) => keys.get(rest[0], Some(rest[1])).map(Some),
        ("keys", "PATCH", [_]) => keys.update(rest[0], None).map(Some),
        ("keys", "PATCH", [_, _]) => keys.update(rest[0], Some(rest[1])).map(Some),
        ("keys", "DELETE", [_]) => keys.delete(rest[0]).map(Some),
        ("deletedkeys", "GET", []) => keys.list_deleted().map(Some),
        ("deletedkeys", "GET", [_]) => keys.get_deleted(rest[0]).map(Some),
        ("deletedkeys", "DELETE", [_]) => keys.purge(rest[0]).map(|()| None),
        ("deletedkeys", "POST", [_, "recover"]) => keys.recover(rest[0]).map(Some),
        ("rng", "POST", []) => keys.random().map(Some),
        _ => Err(ArmError::not_implemented("this Key Vault keys operation")),
    }
}

impl Keys<'_> {
    fn base(&self) -> String {
        self.engine.config().vault_uri(self.vault)
    }

    fn with_vault<T>(&self, f: impl FnOnce(&mut Vault) -> Result<T, ArmError>) -> Result<T, ArmError> {
        let state = self.engine.state::<KeyVaultState>();
        let mut state = state.lock();
        let vault = state.vaults.get_mut(self.vault).ok_or_else(|| ArmError::not_found("VaultNotFound", "The vault was not found."))?;
        f(vault)
    }

    fn attributes(&self, version: &KeyVersion) -> Value {
        let mut out = Value::Object(version.attributes.clone());
        out["enabled"] = version.attributes.get("enabled").cloned().unwrap_or(json!(true));
        out["created"] = json!(version.created);
        out["updated"] = json!(version.updated);
        out["recoveryLevel"] = json!("Recoverable+Purgeable");
        out["recoverableDays"] = json!(RECOVERABLE_DAYS);
        out
    }

    /// A key bundle: the public JSON Web Key, attributes and tags.
    fn bundle(&self, key: &Key, version: &KeyVersion) -> Value {
        let mut jwk = Map::new();
        jwk.insert("kid".into(), json!(format!("{}keys/{}/{}", self.base(), key.name, version.version)));
        jwk.insert("kty".into(), json!(version.kty));
        jwk.insert("key_ops".into(), json!(version.ops));
        if let Some(material) = &version.material {
            jwk.extend(material.public_jwk());
        }
        let mut out = json!({"key": jwk, "attributes": self.attributes(version)});
        if let Some(tags) = &version.tags {
            out["tags"] = tags.clone();
        }
        if let Some(policy) = &version.release_policy {
            out["release_policy"] = policy.clone();
        }
        out
    }

    /// A list item: the key's id (with the version when `versioned`).
    fn item(&self, key: &Key, version: &KeyVersion, versioned: bool) -> Value {
        let kid = match versioned {
            true => format!("{}keys/{}/{}", self.base(), key.name, version.version),
            false => format!("{}keys/{}", self.base(), key.name),
        };
        let mut out = json!({"kid": kid, "attributes": self.attributes(version)});
        if let Some(tags) = &version.tags {
            out["tags"] = tags.clone();
        }
        out
    }

    fn deleted_bundle(&self, deleted: &DeletedKey) -> Value {
        let current = deleted.key.versions.last().expect("a key has a version");
        let mut out = self.bundle(&deleted.key, current);
        out["recoveryId"] = json!(format!("{}deletedkeys/{}", self.base(), deleted.key.name));
        out["deletedDate"] = json!(deleted.deleted_date);
        out["scheduledPurgeDate"] = json!(deleted.deleted_date + RECOVERABLE_DAYS * 86_400);
        out
    }

    /// Attributes and tags from a request body, over `attributes` already set.
    fn apply(&self, version: &mut KeyVersion) {
        if let Some(given) = self.body.get("attributes").and_then(Value::as_object) {
            for member in ["enabled", "nbf", "exp", "exportable"] {
                match given.get(member) {
                    Some(Value::Null) => {
                        version.attributes.remove(member);
                    }
                    Some(value) => {
                        version.attributes.insert(member.into(), value.clone());
                    }
                    None => {}
                }
            }
        }
        if let Some(tags) = self.body.get("tags") {
            version.tags = Some(tags.clone()).filter(|t| !t.is_null());
        }
        if let Some(policy) = self.body.get("release_policy") {
            version.release_policy = Some(policy.clone()).filter(|p| !p.is_null());
        }
    }

    /// The `key_ops` a request asks for, checked against the key type.
    fn requested_ops(&self, kty: &str) -> Result<Vec<String>, ArmError> {
        let applicable = applicable_ops(kty);
        match self.body.get("key_ops").and_then(Value::as_array) {
            None => Ok(applicable.iter().map(|op| op.to_string()).collect()),
            Some(ops) => ops
                .iter()
                .map(|op| {
                    let op = op.as_str().unwrap_or("");
                    if applicable.contains(&op) || op == "import" {
                        Ok(op.to_string())
                    } else {
                        Err(bad(format!("The operation '{op}' does not apply to a {kty} key.")))
                    }
                })
                .collect(),
        }
    }

    /// Stores a new version of `name`, refusing a name that is soft-deleted.
    fn add_version(&self, name: &str, version: KeyVersion) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let lower = name.to_ascii_lowercase();
            if vault.deleted_keys.contains_key(&lower) {
                return Err(ArmError::conflict(
                    "Conflict",
                    format!("Key {name} is currently in a deleted but recoverable state, and its name cannot be reused; in this state, the key can only be recovered or purged."),
                ));
            }
            let key = vault.keys.entry(lower).or_insert_with(|| Key { name: name.to_string(), versions: Vec::new(), rotation_policy: None });
            key.versions.push(version);
            Ok(self.bundle(key, key.versions.last().expect("just pushed")))
        })
    }

    fn premium(&self) -> bool {
        let state = self.engine.state::<emulator_core::arm::ArmState>();
        let state = state.lock();
        let found = state.resources().any(|r| {
            r.resource_type.eq_ignore_ascii_case("Microsoft.KeyVault/vaults")
                && r.name.eq_ignore_ascii_case(self.vault)
                && r.properties.pointer("/sku/name").and_then(Value::as_str).is_some_and(|s| s.eq_ignore_ascii_case("premium"))
        });
        found
    }

    fn create(&self, name: &str) -> Result<Value, ArmError> {
        let kty = self.body.get("kty").and_then(Value::as_str).ok_or_else(|| bad("The parameter 'kty' is required."))?.to_string();
        match kty.as_str() {
            "RSA" | "EC" => {}
            "RSA-HSM" | "EC-HSM" if self.premium() => {}
            "RSA-HSM" | "EC-HSM" => return Err(bad("HSM-protected keys require a Premium vault.")),
            other => return Err(bad(format!("The key type '{other}' is not supported in a vault."))),
        }
        let size = self.body.get("key_size").and_then(Value::as_i64);
        let curve = self.body.get("crv").and_then(Value::as_str);
        let (key_size, curve) = if kty.starts_with("RSA") {
            let size = size.unwrap_or(2048);
            if ![2048, 3072, 4096].contains(&size) {
                return Err(bad("An RSA key's size must be 2048, 3072 or 4096."));
            }
            (Some(size), None)
        } else {
            let curve = curve.unwrap_or("P-256").to_string();
            (None, Some(curve))
        };
        let exponent = self.body.get("public_exponent").and_then(Value::as_i64);
        let material = Material::generate(&kty, key_size, curve.as_deref(), exponent).map_err(bad)?;
        let created = now();
        let mut version = KeyVersion {
            version: uuid::Uuid::new_v4().simple().to_string(),
            ops: self.requested_ops(&kty)?,
            kty,
            key_size,
            curve,
            attributes: Map::new(),
            tags: None,
            rotation_policy: None,
            release_policy: None,
            created,
            updated: created,
            material: Some(material),
        };
        self.apply(&mut version);
        self.add_version(name, version)
    }

    fn import(&self, name: &str) -> Result<Value, ArmError> {
        let jwk = self.body.get("key").ok_or_else(|| bad("The parameter 'key' is required."))?;
        let material = Material::from_jwk(jwk).map_err(bad)?;
        let declared = jwk.get("kty").and_then(Value::as_str).unwrap_or(material.family());
        // `Hsm: true` asks for an HSM-protected key.
        let hsm = self.body.get("Hsm").and_then(Value::as_bool) == Some(true) || declared.ends_with("-HSM");
        if hsm && !self.premium() {
            return Err(bad("HSM-protected keys require a Premium vault."));
        }
        let kty = if hsm { format!("{}-HSM", material.family()) } else { material.family().to_string() };
        let ops = match jwk.get("key_ops").and_then(Value::as_array) {
            Some(ops) => ops.iter().filter_map(Value::as_str).map(str::to_string).collect(),
            None => applicable_ops(&kty).iter().map(|op| op.to_string()).collect(),
        };
        let created = now();
        let mut version = KeyVersion {
            version: uuid::Uuid::new_v4().simple().to_string(),
            kty,
            key_size: material.key_size(),
            curve: material.curve().map(str::to_string),
            ops,
            attributes: Map::new(),
            tags: None,
            rotation_policy: None,
            release_policy: None,
            created,
            updated: created,
            material: Some(material),
        };
        self.apply(&mut version);
        self.add_version(name, version)
    }

    /// A new version with the current version's type, size, curve and operations.
    fn rotate(&self, name: &str) -> Result<Value, ArmError> {
        let current = self.with_vault(|vault| {
            let key = vault.keys.get(&name.to_ascii_lowercase()).ok_or_else(|| key_not_found(name))?;
            Ok(key.versions.last().expect("a key has a version").clone())
        })?;
        let material = Material::generate(&current.kty, current.key_size, current.curve.as_deref(), None).map_err(bad)?;
        let created = now();
        let version = KeyVersion {
            version: uuid::Uuid::new_v4().simple().to_string(),
            attributes: current.attributes.iter().filter(|(k, _)| k.as_str() == "exportable").map(|(k, v)| (k.clone(), v.clone())).collect(),
            created,
            updated: created,
            material: Some(material),
            ..current
        };
        self.add_version(name, version)
    }

    fn get(&self, name: &str, version: Option<&str>) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let key = vault.keys.get(&name.to_ascii_lowercase()).ok_or_else(|| key_not_found(name))?;
            let found = find_version(key, version).ok_or_else(|| key_not_found(&format!("{name}/{}", version.unwrap_or(""))))?;
            Ok(self.bundle(key, found))
        })
    }

    fn update(&self, name: &str, version: Option<&str>) -> Result<Value, ArmError> {
        let ops = self.body.get("key_ops").and_then(Value::as_array).cloned();
        self.with_vault(|vault| {
            let key = vault.keys.get_mut(&name.to_ascii_lowercase()).ok_or_else(|| key_not_found(name))?;
            let index = match version {
                None => key.versions.len() - 1,
                Some(wanted) => key
                    .versions
                    .iter()
                    .position(|v| v.version == wanted)
                    .ok_or_else(|| key_not_found(&format!("{name}/{wanted}")))?,
            };
            let mut target = key.versions[index].clone();
            if let Some(ops) = &ops {
                let applicable = applicable_ops(&target.kty);
                let mut chosen = Vec::new();
                for op in ops {
                    let op = op.as_str().unwrap_or("");
                    if !applicable.contains(&op) && op != "import" {
                        return Err(bad(format!("The operation '{op}' does not apply to a {} key.", target.kty)));
                    }
                    chosen.push(op.to_string());
                }
                target.ops = chosen;
            }
            self.apply(&mut target);
            target.updated = now();
            key.versions[index] = target;
            Ok(self.bundle(key, &key.versions[index]))
        })
    }

    fn list(&self) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let items: Vec<Value> = vault
                .keys
                .values()
                .filter_map(|k| Some(self.item(k, k.versions.last()?, false)))
                .collect();
            Ok(json!({"value": items, "nextLink": null}))
        })
    }

    fn versions(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let items: Vec<Value> = vault
                .keys
                .get(&name.to_ascii_lowercase())
                .map(|k| k.versions.iter().map(|v| self.item(k, v, true)).collect())
                .unwrap_or_default();
            Ok(json!({"value": items, "nextLink": null}))
        })
    }

    fn delete(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let lower = name.to_ascii_lowercase();
            let key = vault.keys.remove(&lower).ok_or_else(|| key_not_found(name))?;
            let deleted = DeletedKey { key, deleted_date: now() };
            let body = self.deleted_bundle(&deleted);
            vault.deleted_keys.insert(lower, deleted);
            Ok(body)
        })
    }

    fn get_deleted(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            vault
                .deleted_keys
                .get(&name.to_ascii_lowercase())
                .map(|d| self.deleted_bundle(d))
                .ok_or_else(|| deleted_not_found(name))
        })
    }

    fn list_deleted(&self) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let items: Vec<Value> = vault
                .deleted_keys
                .values()
                .map(|d| {
                    let current = d.key.versions.last().expect("a key has a version");
                    let mut item = self.item(&d.key, current, false);
                    item["recoveryId"] = json!(format!("{}deletedkeys/{}", self.base(), d.key.name));
                    item["deletedDate"] = json!(d.deleted_date);
                    item["scheduledPurgeDate"] = json!(d.deleted_date + RECOVERABLE_DAYS * 86_400);
                    item
                })
                .collect();
            Ok(json!({"value": items, "nextLink": null}))
        })
    }

    fn purge(&self, name: &str) -> Result<(), ArmError> {
        self.with_vault(|vault| {
            vault.deleted_keys.remove(&name.to_ascii_lowercase()).map(|_| ()).ok_or_else(|| deleted_not_found(name))
        })
    }

    fn recover(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let lower = name.to_ascii_lowercase();
            let deleted = vault.deleted_keys.remove(&lower).ok_or_else(|| deleted_not_found(name))?;
            let body = self.bundle(&deleted.key, deleted.key.versions.last().expect("a key has a version"));
            vault.keys.insert(lower, deleted.key);
            Ok(body)
        })
    }

    /// encrypt, decrypt, sign, verify, wrapKey and unwrapKey.
    fn crypto(&self, name: &str, version: Option<&str>, operation: &str) -> Result<Value, ArmError> {
        let (key_name, found) = self.with_vault(|vault| {
            let key = vault.keys.get(&name.to_ascii_lowercase()).ok_or_else(|| key_not_found(name))?;
            let found = find_version(key, version).ok_or_else(|| key_not_found(&format!("{name}/{}", version.unwrap_or(""))))?;
            Ok((key.name.clone(), found.clone()))
        })?;
        // Unverified against Azure: the codes for a disabled key and a forbidden operation.
        let forbidden = |message: String| ArmError::new(StatusCode::FORBIDDEN, "Forbidden", message);
        if found.attributes.get("enabled") == Some(&json!(false)) {
            return Err(forbidden(format!("Operation {operation} is not allowed on a disabled key.")));
        }
        if !found.ops.iter().any(|op| op == operation) {
            return Err(forbidden(format!("Operation {operation} is not permitted on this key.")));
        }
        let material = found.material.as_ref().ok_or_else(|| ArmError::not_implemented("operations on symmetric keys"))?;
        let alg = self.body.get("alg").and_then(Value::as_str).ok_or_else(|| bad("The parameter 'alg' is required."))?;
        let value = |member: &str| -> Result<Vec<u8>, ArmError> {
            let text = self.body.get(member).and_then(Value::as_str).ok_or_else(|| bad(format!("The parameter '{member}' is required.")))?;
            crypto::unb64(text).map_err(bad)
        };
        let kid = format!("{}keys/{}/{}", self.base(), key_name, found.version);
        let result = match operation {
            "encrypt" | "wrapKey" => material.encrypt(alg, &value("value")?),
            "decrypt" | "unwrapKey" => material.decrypt(alg, &value("value")?),
            "sign" => material.sign(alg, &value("value")?),
            _ => {
                let verified = material.verify(alg, &value("digest")?, &value("value")?).map_err(bad)?;
                return Ok(json!({"value": verified}));
            }
        }
        .map_err(bad)?;
        Ok(json!({"kid": kid, "value": crypto::encode(&result)}))
    }

    /// Get or set a key's rotation policy. Policies are stored and returned but
    /// not acted on: keys rotate only when asked to.
    fn rotation_policy(&self, name: &str, given: Option<&Value>) -> Result<Value, ArmError> {
        let id = format!("{}keys/{name}/rotationpolicy", self.base());
        self.with_vault(|vault| {
            let key = vault.keys.get_mut(&name.to_ascii_lowercase()).ok_or_else(|| key_not_found(name))?;
            if let Some(given) = given {
                let actions = given.get("lifetimeActions").cloned().unwrap_or_else(|| json!([]));
                if !actions.is_array() {
                    return Err(bad("The property 'lifetimeActions' must be an array."));
                }
                let stamp = now();
                let created = key
                    .rotation_policy
                    .as_ref()
                    .and_then(|p| p.pointer("/attributes/created").cloned())
                    .unwrap_or(json!(stamp));
                let mut attributes = json!({"created": created, "updated": stamp});
                if let Some(expiry) = given.pointer("/attributes/expiryTime").filter(|v| !v.is_null()) {
                    attributes["expiryTime"] = expiry.clone();
                }
                key.rotation_policy = Some(json!({"id": id, "lifetimeActions": actions, "attributes": attributes}));
            }
            // Without one, a key has the default policy: notify 30 days before expiry.
            Ok(key.rotation_policy.clone().unwrap_or_else(|| {
                json!({
                    "id": id,
                    "lifetimeActions": [{"trigger": {"timeBeforeExpiry": "P30D"}, "action": {"type": "Notify"}}],
                    "attributes": {},
                })
            }))
        })
    }

    /// Get Random Bytes: `count` bytes from the system's generator.
    fn random(&self) -> Result<Value, ArmError> {
        use rand::RngCore;
        let count = self.body.get("count").and_then(Value::as_u64).ok_or_else(|| bad("The parameter 'count' is required."))?;
        if !(1..=128).contains(&count) {
            return Err(bad("The count must be between 1 and 128."));
        }
        let mut bytes = vec![0u8; count as usize];
        rand::thread_rng().fill_bytes(&mut bytes);
        Ok(json!({"value": crypto::encode(&bytes)}))
    }
}

fn find_version<'k>(key: &'k Key, version: Option<&str>) -> Option<&'k KeyVersion> {
    match version.filter(|v| !v.is_empty()) {
        None => key.versions.last(),
        Some(wanted) => key.versions.iter().find(|v| v.version == wanted),
    }
}
