//! The Key Vault keys data plane, on `{vault}.vault.<suffix>/keys`.
//!
//! Implemented: create (RSA 2048/3072/4096 with any public exponent; EC on
//! P-256, P-256K, P-384 and P-521), import (JSON Web Key with its private
//! part), get (latest or by version), update, list, list versions, delete
//! (soft), get and list deleted, recover, purge, backup and restore, rotate
//! and the rotation policy, and the cryptographic operations: encrypt,
//! decrypt, wrap and unwrap (`RSA1_5`, `RSA-OAEP`, `RSA-OAEP-256`), sign and
//! verify (`RS*`, `PS*`, `ES256`, `ES256K`, `ES384`, `ES512`).
//!
//! Keys are the same ones Resource Manager lists under
//! `Microsoft.KeyVault/vaults/keys`. Listings return every item in one page.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use emulator_core::{ArmError, Engine};
use http::{Method, StatusCode};
use serde_json::{json, Map, Value};

use crate::crypto::{unb64, Curve, Material};
use crate::keys::{applicable_ops, Key, KeyVersion};
use crate::KeyVaultState;

/// Days a deleted key stays recoverable.
const RECOVERABLE_DAYS: i64 = 90;
/// What a backup blob starts with, so a restore can tell one from noise.
const BACKUP_MARKER: &str = "azure-emulator-key-backup-v1";

/// What answers a keys request: a JSON body, or an empty one with a status.
pub(crate) enum Answer {
    Json(Value),
    Empty(StatusCode),
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn bad(message: impl Into<String>) -> ArmError {
    ArmError::bad_request("BadParameter", message)
}

fn forbidden(message: impl Into<String>) -> ArmError {
    ArmError::new(StatusCode::FORBIDDEN, "Forbidden", message)
}

fn key_not_found(name: &str) -> ArmError {
    ArmError::not_found("KeyNotFound", format!("A key with (name/id) {name} was not found in this key vault. If you recently deleted this key you may be able to recover it using the correct recovery command. For help resolving this issue, please see https://go.microsoft.com/fwlink/?linkid=2125182"))
}

fn deleted_not_found(name: &str) -> ArmError {
    ArmError::not_found("KeyNotFound", format!("Deleted Key not found: {name}"))
}

fn in_deleted_state(name: &str) -> ArmError {
    ArmError::conflict(
        "Conflict",
        format!("Key {name} is currently in a deleted but recoverable state, and its name cannot be reused; in this state, the key can only be recovered or purged."),
    )
}

/// What an operation needs: its data action under role-based access control,
/// and its permission under an access policy.
pub(crate) fn permission_for(method: &Method, collection: &str, rest: &[&str]) -> Option<(&'static str, &'static str)> {
    macro_rules! action {
        ($name:literal) => {
            concat!("Microsoft.KeyVault/vaults/keys/", $name)
        };
    }
    let lower: Vec<String> = rest.iter().map(|s| s.to_ascii_lowercase()).collect();
    let rest: Vec<&str> = lower.iter().map(String::as_str).collect();
    Some(match (collection, rest.as_slice(), method.as_str()) {
        ("keys", [], "GET") | ("keys", [_, "versions"], "GET") => (action!("read"), "list"),
        ("keys", ["restore"], "POST") => (action!("restore/action"), "restore"),
        ("keys", [_, "create"], "POST") => (action!("create/action"), "create"),
        ("keys", [_], "PUT") => (action!("import/action"), "import"),
        ("keys", [_, "backup"], "POST") => (action!("backup/action"), "backup"),
        ("keys", [_, "rotate"], "POST") => (action!("rotate/action"), "rotate"),
        ("keys", [_, "rotationpolicy"], "GET") => ("Microsoft.KeyVault/vaults/keyrotationpolicies/read", "getrotationpolicy"),
        ("keys", [_, "rotationpolicy"], "PUT") => ("Microsoft.KeyVault/vaults/keyrotationpolicies/write", "setrotationpolicy"),
        ("keys", [_] | [_, _], "GET") => (action!("read"), "get"),
        ("keys", [_] | [_, _], "PATCH") => (action!("update/action"), "update"),
        ("keys", [_], "DELETE") => (action!("delete"), "delete"),
        ("keys", [_, op] | [_, _, op], "POST") => match *op {
            "encrypt" => (action!("encrypt/action"), "encrypt"),
            "decrypt" => (action!("decrypt/action"), "decrypt"),
            "wrapkey" => (action!("wrap/action"), "wrapKey"),
            "unwrapkey" => (action!("unwrap/action"), "unwrapKey"),
            "sign" => (action!("sign/action"), "sign"),
            "verify" => (action!("verify/action"), "verify"),
            _ => return None,
        },
        ("deletedkeys", [] | [_], "GET") => (action!("read"), "list"),
        ("deletedkeys", [_], "DELETE") => (action!("purge/action"), "purge"),
        ("deletedkeys", [_, "recover"], "POST") => (action!("recover/action"), "recover"),
        _ => return None,
    })
}

pub(crate) struct KeyRequest<'a> {
    pub engine: &'a Engine,
    pub vault: &'a str,
    pub body: &'a Value,
}

impl KeyRequest<'_> {
    pub(crate) fn serve(&self, method: &Method, collection: &str, rest: &[&str]) -> Result<Answer, ArmError> {
        let lower: Vec<String> = rest.iter().map(|s| s.to_ascii_lowercase()).collect();
        let tail: Vec<&str> = lower.iter().map(String::as_str).collect();
        let json = |result: Result<Value, ArmError>| result.map(Answer::Json);
        match (collection, method.as_str(), rest, tail.as_slice()) {
            ("keys", "GET", [], _) => json(self.list()),
            ("keys", "POST", ["restore"], _) => json(self.restore()),
            ("keys", "POST", [name, _], [_, "create"]) => json(self.create(name)),
            ("keys", "PUT", [name], _) => json(self.import(name)),
            ("keys", "GET", [name], _) => json(self.get(name, None)),
            ("keys", "PATCH", [name], _) => json(self.update(name, None)),
            ("keys", "DELETE", [name], _) => json(self.delete(name)),
            ("keys", "GET", [name, _], [_, "versions"]) => json(self.versions(name)),
            ("keys", "POST", [name, _], [_, "backup"]) => json(self.backup(name)),
            ("keys", "POST", [name, _], [_, "rotate"]) => json(self.rotate(name)),
            ("keys", "GET", [name, _], [_, "rotationpolicy"]) => json(self.rotation_policy(name)),
            ("keys", "PUT", [name, _], [_, "rotationpolicy"]) => json(self.set_rotation_policy(name)),
            ("keys", "GET", [name, version], _) => json(self.get(name, Some(version))),
            ("keys", "PATCH", [name, version], _) => json(self.update(name, Some(version))),
            // `keys/{name}/{version}/{operation}`; the SDKs leave the version
            // out (an empty segment) to use the latest one.
            ("keys", "POST", [name, op], [_, _]) => json(self.operate(name, None, op)),
            ("keys", "POST", [name, version, op], _) => json(self.operate(name, Some(version), op)),
            ("deletedkeys", "GET", [], _) => json(self.list_deleted()),
            ("deletedkeys", "GET", [name], _) => json(self.get_deleted(name)),
            ("deletedkeys", "DELETE", [name], _) => self.purge(name).map(|()| Answer::Empty(StatusCode::NO_CONTENT)),
            ("deletedkeys", "POST", [name, _], [_, "recover"]) => json(self.recover(name)),
            _ => Err(ArmError::not_implemented("this Key Vault operation")),
        }
    }

    fn base(&self) -> String {
        self.engine.config().vault_uri(self.vault)
    }

    fn with_vault<T>(&self, f: impl FnOnce(&mut crate::secrets::Vault) -> Result<T, ArmError>) -> Result<T, ArmError> {
        let state = self.engine.state::<KeyVaultState>();
        let mut state = state.lock();
        let vault = state.vaults.get_mut(self.vault).ok_or_else(|| ArmError::not_found("VaultNotFound", "The vault was not found."))?;
        f(vault)
    }

    fn kid(&self, key: &Key, version: &KeyVersion) -> String {
        format!("{}keys/{}/{}", self.base(), key.name, version.version)
    }

    fn attributes(version: &KeyVersion) -> Value {
        let mut attributes = Value::Object(version.attributes.clone());
        attributes["enabled"] = version.attributes.get("enabled").cloned().unwrap_or(json!(true));
        attributes["created"] = json!(version.created);
        attributes["updated"] = json!(version.updated);
        attributes["recoveryLevel"] = json!("Recoverable+Purgeable");
        attributes["recoverableDays"] = json!(RECOVERABLE_DAYS);
        attributes
    }

    /// A key bundle: the public key as a JSON Web Key, with its attributes and tags.
    fn bundle(&self, key: &Key, version: &KeyVersion) -> Value {
        let mut jwk = Map::new();
        jwk.insert("kid".into(), json!(self.kid(key, version)));
        jwk.insert("kty".into(), json!(version.kty));
        jwk.insert("key_ops".into(), json!(version.ops));
        jwk.extend(version.material.public_jwk());
        let mut out = json!({"key": jwk, "attributes": Self::attributes(version)});
        if let Some(tags) = &version.tags {
            out["tags"] = tags.clone();
        }
        if let Some(policy) = &version.release_policy {
            out["release_policy"] = policy.clone();
        }
        out
    }

    fn deleted_bundle(&self, key: &Key, deleted: i64) -> Value {
        let mut out = self.bundle(key, key.versions.last().expect("a key has a version"));
        out["recoveryId"] = json!(format!("{}deletedkeys/{}", self.base(), key.name));
        out["deletedDate"] = json!(deleted);
        out["scheduledPurgeDate"] = json!(deleted + RECOVERABLE_DAYS * 86_400);
        out
    }

    fn item(&self, key: &Key, version: &KeyVersion, versioned: bool) -> Value {
        let kid = match versioned {
            true => self.kid(key, version),
            false => format!("{}keys/{}", self.base(), key.name),
        };
        let mut out = json!({"kid": kid, "attributes": Self::attributes(version)});
        if let Some(tags) = &version.tags {
            out["tags"] = tags.clone();
        }
        out
    }

    /// `attributes` from a request: `enabled`, `nbf`, `exp`, `exportable`.
    fn given_attributes(&self) -> Map<String, Value> {
        let mut out = Map::new();
        for member in ["enabled", "nbf", "exp", "exportable"] {
            if let Some(value) = self.body.pointer(&format!("/attributes/{member}")).filter(|v| !v.is_null()) {
                out.insert(member.into(), value.clone());
            }
        }
        out
    }

    fn given_ops(&self, kty: &str, given: Option<&Value>) -> Result<Vec<String>, ArmError> {
        let applicable = applicable_ops(kty);
        match given.and_then(Value::as_array) {
            Some(requested) => requested
                .iter()
                .map(|op| {
                    let op = op.as_str().unwrap_or("");
                    match applicable.iter().find(|a| a.eq_ignore_ascii_case(op)) {
                        Some(found) => Ok(found.to_string()),
                        None if op == "import" => Ok(op.to_string()),
                        None => Err(bad(format!("Invalid key operation: {op} for a key of type {kty}."))),
                    }
                })
                .collect(),
            None => Ok(applicable.iter().map(|op| op.to_string()).collect()),
        }
    }

    /// Adds `version` to the key `name`, creating the key if need be.
    fn add_version(&self, name: &str, version: KeyVersion) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let lower = name.to_ascii_lowercase();
            if vault.deleted_keys.contains_key(&lower) {
                return Err(in_deleted_state(name));
            }
            let key = vault.keys.entry(lower).or_insert_with(|| Key { name: name.to_string(), versions: Vec::new() });
            key.versions.push(version);
            Ok(self.bundle(key, key.versions.last().expect("just pushed")))
        })
    }

    fn create(&self, name: &str) -> Result<Value, ArmError> {
        let body = self.body;
        let kty = body.get("kty").and_then(Value::as_str).ok_or_else(|| bad("Key type (kty) is required."))?.to_string();
        let (material, key_size, curve) = match kty.as_str() {
            "RSA" | "RSA-HSM" => {
                let size = body.get("key_size").and_then(Value::as_i64).unwrap_or(2048);
                if ![2048, 3072, 4096].contains(&size) {
                    return Err(bad(format!("Invalid key size {size}. Supported sizes are 2048, 3072 and 4096.")));
                }
                let exponent = body.get("public_exponent").and_then(Value::as_u64);
                let material = Material::new_rsa(size as usize, exponent).map_err(bad)?;
                (material, Some(size), None)
            }
            "EC" | "EC-HSM" => {
                let name = body.get("crv").and_then(Value::as_str).unwrap_or("P-256");
                let curve = Curve::from_name(name).ok_or_else(|| bad(format!("Invalid curve name {name}.")))?;
                (Material::new_ec(curve), None, Some(curve.name().to_string()))
            }
            other => return Err(bad(format!("Invalid key type {other} for a vault. Supported types are RSA, RSA-HSM, EC and EC-HSM."))),
        };
        let created = now();
        let version = KeyVersion {
            version: uuid::Uuid::new_v4().simple().to_string(),
            ops: self.given_ops(&kty, body.get("key_ops"))?,
            kty,
            key_size,
            curve,
            attributes: self.given_attributes(),
            tags: body.get("tags").filter(|t| !t.is_null()).cloned(),
            rotation_policy: None,
            release_policy: body.get("release_policy").filter(|v| !v.is_null()).cloned(),
            created,
            updated: created,
            material,
        };
        self.add_version(name, version)
    }

    fn import(&self, name: &str) -> Result<Value, ArmError> {
        let jwk = self.body.get("key").filter(|k| k.is_object()).ok_or_else(|| bad("The key to import (key) is required."))?;
        let material = Material::from_jwk(jwk).map_err(bad)?;
        let base = match &material {
            Material::Rsa(_) => "RSA",
            Material::Ec { .. } => "EC",
            Material::Oct(_) => return Err(bad("Symmetric keys cannot be imported into a vault.")),
        };
        let hsm = self.body.get("Hsm").and_then(Value::as_bool) == Some(true)
            || jwk.get("kty").and_then(Value::as_str).is_some_and(|k| k.ends_with("-HSM"));
        let kty = if hsm { format!("{base}-HSM") } else { base.to_string() };
        let public = material.public_jwk();
        let key_size = public.get("n").and_then(Value::as_str).and_then(unb64).map(|n| n.len() as i64 * 8);
        let curve = public.get("crv").and_then(Value::as_str).map(str::to_string);
        let created = now();
        let version = KeyVersion {
            version: uuid::Uuid::new_v4().simple().to_string(),
            ops: self.given_ops(&kty, jwk.get("key_ops"))?,
            kty,
            key_size,
            curve,
            attributes: self.given_attributes(),
            tags: self.body.get("tags").filter(|t| !t.is_null()).cloned(),
            rotation_policy: None,
            release_policy: self.body.get("release_policy").filter(|v| !v.is_null()).cloned(),
            created,
            updated: created,
            material,
        };
        self.add_version(name, version)
    }

    fn get(&self, name: &str, version: Option<&str>) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let key = vault.keys.get(&name.to_ascii_lowercase()).ok_or_else(|| key_not_found(name))?;
            let found = find_version(key, version).ok_or_else(|| key_not_found(name))?;
            Ok(self.bundle(key, found))
        })
    }

    fn update(&self, name: &str, version: Option<&str>) -> Result<Value, ArmError> {
        let attributes = self.given_attributes();
        let tags = self.body.get("tags").cloned();
        let ops = self.body.get("key_ops").cloned();
        let release = self.body.get("release_policy").cloned();
        self.with_vault(|vault| {
            let key = vault.keys.get_mut(&name.to_ascii_lowercase()).ok_or_else(|| key_not_found(name))?;
            let index = match version.filter(|v| !v.is_empty()) {
                Some(wanted) => key.versions.iter().position(|v| v.version == wanted).ok_or_else(|| key_not_found(name))?,
                None => key.versions.len() - 1,
            };
            let kty = key.versions[index].kty.clone();
            let new_ops = match ops.filter(|o| !o.is_null()) {
                Some(ops) => Some(self.given_ops(&kty, Some(&ops))?),
                None => None,
            };
            let target = &mut key.versions[index];
            target.attributes.extend(attributes);
            if let Some(tags) = tags {
                target.tags = (!tags.is_null()).then_some(tags);
            }
            if let Some(ops) = new_ops {
                target.ops = ops;
            }
            if let Some(policy) = release.filter(|p| !p.is_null()) {
                target.release_policy = Some(policy);
            }
            target.updated = now();
            Ok(self.bundle(key, &key.versions[index]))
        })
    }

    fn list(&self) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let items: Vec<Value> = vault
                .keys
                .values()
                .map(|key| self.item(key, key.versions.last().expect("a key has a version"), false))
                .collect();
            Ok(json!({"value": items, "nextLink": null}))
        })
    }

    fn versions(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let key = vault.keys.get(&name.to_ascii_lowercase()).ok_or_else(|| key_not_found(name))?;
            let items: Vec<Value> = key.versions.iter().map(|v| self.item(key, v, true)).collect();
            Ok(json!({"value": items, "nextLink": null}))
        })
    }

    fn delete(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let lower = name.to_ascii_lowercase();
            let key = vault.keys.remove(&lower).ok_or_else(|| key_not_found(name))?;
            let deleted = now();
            let body = self.deleted_bundle(&key, deleted);
            vault.deleted_keys.insert(lower, (key, deleted));
            Ok(body)
        })
    }

    fn list_deleted(&self) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let items: Vec<Value> = vault
                .deleted_keys
                .values()
                .map(|(key, deleted)| {
                    let mut item = self.item(key, key.versions.last().expect("a key has a version"), false);
                    item["recoveryId"] = json!(format!("{}deletedkeys/{}", self.base(), key.name));
                    item["deletedDate"] = json!(deleted);
                    item["scheduledPurgeDate"] = json!(deleted + RECOVERABLE_DAYS * 86_400);
                    item
                })
                .collect();
            Ok(json!({"value": items, "nextLink": null}))
        })
    }

    fn get_deleted(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let (key, deleted) = vault.deleted_keys.get(&name.to_ascii_lowercase()).ok_or_else(|| deleted_not_found(name))?;
            Ok(self.deleted_bundle(key, *deleted))
        })
    }

    fn purge(&self, name: &str) -> Result<(), ArmError> {
        self.with_vault(|vault| vault.deleted_keys.remove(&name.to_ascii_lowercase()).map(|_| ()).ok_or_else(|| deleted_not_found(name)))
    }

    fn recover(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let lower = name.to_ascii_lowercase();
            if vault.keys.contains_key(&lower) {
                return Err(ArmError::conflict("Conflict", format!("Key {name} already exists.")));
            }
            let (key, _) = vault.deleted_keys.remove(&lower).ok_or_else(|| deleted_not_found(name))?;
            let body = self.bundle(&key, key.versions.last().expect("a key has a version"));
            vault.keys.insert(lower, key);
            Ok(body)
        })
    }

    /// Backup Key: every version, private material included, in one opaque value.
    fn backup(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let key = vault.keys.get(&name.to_ascii_lowercase()).ok_or_else(|| key_not_found(name))?;
            let versions: Vec<Value> = key
                .versions
                .iter()
                .map(|v| {
                    let mut jwk = v.material.private_jwk();
                    jwk.insert("kty".into(), json!(v.kty));
                    json!({
                        "version": v.version, "kty": v.kty, "key_size": v.key_size, "curve": v.curve, "ops": v.ops,
                        "attributes": v.attributes, "tags": v.tags, "rotation_policy": v.rotation_policy,
                        "release_policy": v.release_policy, "created": v.created, "updated": v.updated, "jwk": jwk,
                    })
                })
                .collect();
            let blob = json!({"format": BACKUP_MARKER, "name": key.name, "versions": versions});
            Ok(json!({"value": URL_SAFE_NO_PAD.encode(blob.to_string())}))
        })
    }

    fn restore(&self) -> Result<Value, ArmError> {
        let malformed = || bad("Backup blob contains invalid or corrupt version.");
        let blob: Value = self
            .body
            .get("value")
            .and_then(Value::as_str)
            .and_then(unb64)
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .filter(|b: &Value| b.get("format").and_then(Value::as_str) == Some(BACKUP_MARKER))
            .ok_or_else(malformed)?;
        let name = blob.get("name").and_then(Value::as_str).ok_or_else(malformed)?.to_string();
        let mut versions = Vec::new();
        for v in blob.get("versions").and_then(Value::as_array).ok_or_else(malformed)? {
            let text = |member: &str| v.get(member).and_then(Value::as_str).map(str::to_string);
            versions.push(KeyVersion {
                version: text("version").ok_or_else(malformed)?,
                kty: text("kty").ok_or_else(malformed)?,
                key_size: v.get("key_size").and_then(Value::as_i64),
                curve: text("curve"),
                ops: serde_json::from_value(v.get("ops").cloned().unwrap_or_default()).map_err(|_| malformed())?,
                attributes: v.get("attributes").and_then(Value::as_object).cloned().unwrap_or_default(),
                tags: v.get("tags").filter(|t| !t.is_null()).cloned(),
                rotation_policy: v.get("rotation_policy").filter(|t| !t.is_null()).cloned(),
                release_policy: v.get("release_policy").filter(|t| !t.is_null()).cloned(),
                created: v.get("created").and_then(Value::as_i64).unwrap_or_else(now),
                updated: v.get("updated").and_then(Value::as_i64).unwrap_or_else(now),
                material: Material::from_jwk(v.get("jwk").ok_or_else(malformed)?).map_err(|_| malformed())?,
            });
        }
        if versions.is_empty() {
            return Err(malformed());
        }
        self.with_vault(|vault| {
            let lower = name.to_ascii_lowercase();
            if vault.keys.contains_key(&lower) {
                return Err(ArmError::conflict("Conflict", format!("Key {name} already exists. Restore is not allowed over an existing key.")));
            }
            if vault.deleted_keys.contains_key(&lower) {
                return Err(in_deleted_state(&name));
            }
            let key = Key { name, versions };
            let body = self.bundle(&key, key.versions.last().expect("checked above"));
            vault.keys.insert(lower, key);
            Ok(body)
        })
    }

    /// Rotate Key: a new version of the same type, size and operations.
    fn rotate(&self, name: &str) -> Result<Value, ArmError> {
        let current = self.with_vault(|vault| {
            let key = vault.keys.get(&name.to_ascii_lowercase()).ok_or_else(|| key_not_found(name))?;
            Ok(key.versions.last().expect("a key has a version").clone())
        })?;
        let material = match (&current.material, current.curve.as_deref().and_then(Curve::from_name)) {
            (Material::Ec { .. }, Some(curve)) => Material::new_ec(curve),
            (Material::Rsa(_), _) => Material::new_rsa(current.key_size.unwrap_or(2048) as usize, None).map_err(bad)?,
            _ => return Err(bad("This key cannot be rotated.")),
        };
        let created = now();
        let mut attributes = current.attributes.clone();
        attributes.remove("nbf");
        attributes.remove("exp");
        let version = KeyVersion {
            version: uuid::Uuid::new_v4().simple().to_string(),
            attributes,
            created,
            updated: created,
            material,
            ..current
        };
        self.add_version(name, version)
    }

    fn rotation_policy(&self, name: &str) -> Result<Value, ArmError> {
        let id = format!("{}keys/{name}/rotationpolicy", self.base());
        self.with_vault(|vault| {
            let key = vault.keys.get(&name.to_ascii_lowercase()).ok_or_else(|| key_not_found(name))?;
            let current = key.versions.last().expect("a key has a version");
            Ok(current.rotation_policy.clone().unwrap_or_else(|| {
                // The policy every key has until one is set: notify 30 days before expiry.
                json!({
                    "id": id,
                    "lifetimeActions": [{"trigger": {"timeBeforeExpiry": "P30D"}, "action": {"type": "Notify"}}],
                    "attributes": {"created": current.created, "updated": current.created},
                })
            }))
        })
    }

    fn set_rotation_policy(&self, name: &str) -> Result<Value, ArmError> {
        let id = format!("{}keys/{name}/rotationpolicy", self.base());
        let mut policy = json!({
            "id": id,
            "lifetimeActions": self.body.get("lifetimeActions").cloned().unwrap_or(json!([])),
            "attributes": self.body.get("attributes").cloned().unwrap_or(json!({})),
        });
        let stamp = now();
        self.with_vault(|vault| {
            let key = vault.keys.get_mut(&name.to_ascii_lowercase()).ok_or_else(|| key_not_found(name))?;
            let current = key.versions.last_mut().expect("a key has a version");
            let created = current.rotation_policy.as_ref().and_then(|p| p.pointer("/attributes/created")).cloned().unwrap_or(json!(stamp));
            policy["attributes"]["created"] = created;
            policy["attributes"]["updated"] = json!(stamp);
            current.rotation_policy = Some(policy.clone());
            Ok(policy)
        })
    }

    /// Encrypt, decrypt, wrap, unwrap, sign and verify with one version of a key.
    fn operate(&self, name: &str, version: Option<&str>, operation: &str) -> Result<Value, ArmError> {
        let op = match operation.to_ascii_lowercase().as_str() {
            "encrypt" => "encrypt",
            "decrypt" => "decrypt",
            "wrapkey" => "wrapKey",
            "unwrapkey" => "unwrapKey",
            "sign" => "sign",
            "verify" => "verify",
            _ => return Err(ArmError::not_implemented("this Key Vault operation")),
        };
        let (kid, found) = self.with_vault(|vault| {
            let key = vault.keys.get(&name.to_ascii_lowercase()).ok_or_else(|| key_not_found(name))?;
            let found = find_version(key, version).ok_or_else(|| key_not_found(name))?;
            Ok((self.kid(key, found), found.clone()))
        })?;
        let enabled = found.attributes.get("enabled").and_then(Value::as_bool) != Some(false);
        if !enabled {
            return Err(forbidden(format!("Operation {op} is not allowed on a disabled key.")));
        }
        let now = now();
        if found.attributes.get("exp").and_then(Value::as_i64).is_some_and(|exp| exp <= now) {
            return Err(forbidden(format!("Operation {op} is not allowed on an expired key.")));
        }
        if found.attributes.get("nbf").and_then(Value::as_i64).is_some_and(|nbf| nbf > now) {
            return Err(forbidden(format!("Operation {op} is not allowed on a key that is not yet valid.")));
        }
        if !found.ops.iter().any(|o| o == op) {
            return Err(forbidden(format!("Operation {op} is not permitted on this key.")));
        }
        let alg = self.body.get("alg").and_then(Value::as_str).ok_or_else(|| bad("The algorithm (alg) is required."))?;
        let bytes = |member: &str| {
            self.body.get(member).and_then(Value::as_str).and_then(unb64).ok_or_else(|| bad(format!("The '{member}' must be base64url.")))
        };
        let encoded = |data: Vec<u8>| json!({"kid": kid, "value": URL_SAFE_NO_PAD.encode(data)});
        let material = &found.material;
        match op {
            "encrypt" | "wrapKey" => Ok(encoded(material.encrypt(alg, &bytes("value")?).map_err(bad)?)),
            "decrypt" | "unwrapKey" => Ok(encoded(material.decrypt(alg, &bytes("value")?).map_err(bad)?)),
            "sign" => Ok(encoded(material.sign(alg, &bytes("value")?).map_err(bad)?)),
            _ => Ok(json!({"value": material.verify(alg, &bytes("digest")?, &bytes("value")?).map_err(bad)?})),
        }
    }
}

fn find_version<'a>(key: &'a Key, version: Option<&str>) -> Option<&'a KeyVersion> {
    match version.filter(|v| !v.is_empty()) {
        Some(wanted) => key.versions.iter().find(|v| v.version == wanted),
        None => key.versions.last(),
    }
}
