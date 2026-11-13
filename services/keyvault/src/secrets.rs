//! The Key Vault secrets data plane.
//!
//! Implemented: set, get (latest and by version), update attributes, list, list
//! versions, delete (soft), get/list deleted, recover and purge. Listings return
//! every item in one page.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use emulator_core::auth::{self, AuthFailure};
use emulator_core::http_util::{json_response, parse_query, percent_decode, read_body, request_id};
use emulator_core::{ArmError, Engine};
use http::{header, HeaderValue, Method, Request, StatusCode};
use serde_json::{json, Value};

use crate::KeyVaultState;

/// Days a deleted secret stays recoverable.
const RECOVERABLE_DAYS: i64 = 90;

#[derive(Default)]
pub(crate) struct Vault {
    /// Keyed by lowercased secret name.
    secrets: BTreeMap<String, Secret>,
    deleted: BTreeMap<String, DeletedSecret>,
    /// Keyed by lowercased key name.
    pub(crate) keys: crate::keys::KeyStore,
    /// Soft-deleted keys, keyed the same way.
    pub(crate) deleted_keys: BTreeMap<String, crate::keys::DeletedKey>,
    /// The certificate contacts, as last set.
    certificate_contacts: Option<Value>,
}

#[derive(Clone)]
struct Secret {
    /// The name as first created; ids keep this casing.
    name: String,
    /// Oldest first; the last one is the current version.
    versions: Vec<SecretVersion>,
}

#[derive(Clone)]
struct SecretVersion {
    version: String,
    value: String,
    content_type: Option<Value>,
    tags: Option<Value>,
    enabled: bool,
    not_before: Option<Value>,
    expires: Option<Value>,
    created: i64,
    updated: i64,
}

struct DeletedSecret {
    secret: Secret,
    deleted_date: i64,
}

/// A secret's current version as the Resource Manager API presents it: never the
/// value, only what describes it.
pub(crate) struct SecretSummary {
    pub name: String,
    pub version: String,
    pub content_type: Option<Value>,
    pub tags: Option<Value>,
    /// `enabled`, `created`, `updated` and, when set, `nbf` and `exp`.
    pub attributes: Value,
}

impl Vault {
    /// Stores a new version of `name`. `body` carries the optional `contentType`,
    /// `tags` and `attributes` in the data plane's shape. Returns whether the
    /// secret is new.
    pub(crate) fn add_version(&mut self, name: &str, value: &str, body: &Value) -> Result<bool, ArmError> {
        let key = name.to_ascii_lowercase();
        if self.deleted.contains_key(&key) {
            return Err(ArmError::conflict(
                "Conflict",
                format!("Secret {name} is currently in a deleted but recoverable state, and its name cannot be reused; in this state, the secret can only be recovered or purged."),
            ));
        }
        let created = now();
        let mut version = SecretVersion {
            version: uuid::Uuid::new_v4().simple().to_string(),
            value: value.to_string(),
            content_type: None,
            tags: None,
            enabled: true,
            not_before: None,
            expires: None,
            created,
            updated: created,
        };
        version.apply(body);
        let is_new = !self.secrets.contains_key(&key);
        let secret = self.secrets.entry(key).or_insert_with(|| Secret {
            name: name.to_string(),
            versions: Vec::new(),
        });
        secret.versions.push(version);
        Ok(is_new)
    }

    /// Applies `body` (data-plane shape) to the current version of `name`.
    /// Returns `false` if there is no such secret.
    pub(crate) fn update_current(&mut self, name: &str, body: &Value) -> bool {
        let Some(current) = self
            .secrets
            .get_mut(&name.to_ascii_lowercase())
            .and_then(|secret| secret.versions.last_mut())
        else {
            return false;
        };
        current.apply(body);
        current.updated = now();
        true
    }

    fn summarize(secret: &Secret) -> Option<SecretSummary> {
        let current = secret.versions.last()?;
        let mut attributes = current.attributes();
        // Recovery settings belong to the data plane's view only.
        if let Some(members) = attributes.as_object_mut() {
            members.remove("recoveryLevel");
            members.remove("recoverableDays");
        }
        Some(SecretSummary {
            name: secret.name.clone(),
            version: current.version.clone(),
            content_type: current.content_type.clone(),
            tags: current.tags.clone(),
            attributes,
        })
    }

    pub(crate) fn summary(&self, name: &str) -> Option<SecretSummary> {
        Self::summarize(self.secrets.get(&name.to_ascii_lowercase())?)
    }

    pub(crate) fn summaries(&self) -> Vec<SecretSummary> {
        self.secrets.values().filter_map(Self::summarize).collect()
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn not_found(what: &str) -> ArmError {
    ArmError::not_found(
        "SecretNotFound",
        format!("A secret with (name/id) {what} was not found in this key vault. If you recently deleted this secret you may be able to recover it using the correct recovery command. For help resolving this issue, please see https://go.microsoft.com/fwlink/?linkid=2125182"),
    )
}

fn is_valid_secret_name(name: &str) -> bool {
    (1..=127).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

struct SecretRequest<'a> {
    engine: &'a Engine,
    vault: &'a str,
    body: &'a Value,
}

impl SecretVersion {
    fn attributes(&self) -> Value {
        let mut out = json!({
            "enabled": self.enabled,
            "created": self.created,
            "updated": self.updated,
            "recoveryLevel": "Recoverable+Purgeable",
            "recoverableDays": RECOVERABLE_DAYS,
        });
        if let Some(not_before) = &self.not_before {
            out["nbf"] = not_before.clone();
        }
        if let Some(expires) = &self.expires {
            out["exp"] = expires.clone();
        }
        out
    }

    /// Applies the settable members of a set/update request body.
    fn apply(&mut self, body: &Value) {
        if let Some(content_type) = body.get("contentType") {
            self.content_type = Some(content_type.clone()).filter(|v| !v.is_null());
        }
        if let Some(tags) = body.get("tags") {
            self.tags = Some(tags.clone()).filter(|v| !v.is_null());
        }
        let Some(attributes) = body.get("attributes") else {
            return;
        };
        if let Some(enabled) = attributes.get("enabled").and_then(Value::as_bool) {
            self.enabled = enabled;
        }
        if let Some(not_before) = attributes.get("nbf") {
            self.not_before = Some(not_before.clone()).filter(|v| !v.is_null());
        }
        if let Some(expires) = attributes.get("exp") {
            self.expires = Some(expires.clone()).filter(|v| !v.is_null());
        }
    }
}

impl SecretRequest<'_> {
    fn base(&self) -> String {
        self.engine.config().vault_uri(self.vault)
    }

    /// A secret item or bundle. `versioned` puts the version in the id; `value`
    /// includes the secret value.
    fn bundle(&self, name: &str, version: &SecretVersion, versioned: bool, value: bool) -> Value {
        let mut id = format!("{}secrets/{name}", self.base());
        if versioned {
            id.push_str(&format!("/{}", version.version));
        }
        let mut out = json!({"id": id, "attributes": version.attributes()});
        if value {
            out["value"] = json!(version.value);
        }
        if let Some(content_type) = &version.content_type {
            out["contentType"] = content_type.clone();
        }
        if let Some(tags) = &version.tags {
            out["tags"] = tags.clone();
        }
        out
    }

    fn deleted_bundle(&self, deleted: &DeletedSecret, value: bool) -> Value {
        let secret = &deleted.secret;
        let current = secret.versions.last().expect("a secret has a version");
        let mut out = self.bundle(&secret.name, current, true, value);
        out["recoveryId"] = json!(format!("{}deletedsecrets/{}", self.base(), secret.name));
        out["deletedDate"] = json!(deleted.deleted_date);
        out["scheduledPurgeDate"] = json!(deleted.deleted_date + RECOVERABLE_DAYS * 86_400);
        out
    }

    fn with_vault<T>(&self, f: impl FnOnce(&mut Vault) -> Result<T, ArmError>) -> Result<T, ArmError> {
        let state = self.engine.state::<KeyVaultState>();
        let mut state = state.lock();
        let vault = state
            .vaults
            .get_mut(self.vault)
            .ok_or_else(|| ArmError::not_found("VaultNotFound", "The vault was not found."))?;
        f(vault)
    }

    fn set(&self, name: &str) -> Result<Value, ArmError> {
        let value = self.body.get("value").and_then(Value::as_str).ok_or_else(|| {
            ArmError::bad_request("BadParameter", "The property 'value' is required.")
        })?;
        self.with_vault(|vault| {
            vault.add_version(name, value, self.body)?;
            let secret = &vault.secrets[&name.to_ascii_lowercase()];
            Ok(self.bundle(&secret.name, secret.versions.last().expect("just pushed"), true, true))
        })
    }

    fn get(&self, name: &str, version: Option<&str>) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let secret = vault
                .secrets
                .get(&name.to_ascii_lowercase())
                .ok_or_else(|| not_found(name))?;
            let found = match version {
                None => secret.versions.last(),
                Some(wanted) => secret.versions.iter().find(|v| v.version == wanted),
            }
            .ok_or_else(|| not_found(&format!("{name}/{}", version.unwrap_or(""))))?;
            if !found.enabled {
                return Err(ArmError::new(
                    StatusCode::FORBIDDEN,
                    "Forbidden",
                    "Operation get is not allowed on a disabled secret.",
                ));
            }
            Ok(self.bundle(&secret.name, found, true, true))
        })
    }

    fn update(&self, name: &str, version: Option<&str>) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let secret = vault
                .secrets
                .get_mut(&name.to_ascii_lowercase())
                .ok_or_else(|| not_found(name))?;
            let secret_name = secret.name.clone();
            let found = match version {
                None => secret.versions.last_mut(),
                Some(wanted) => secret.versions.iter_mut().find(|v| v.version == wanted),
            }
            .ok_or_else(|| not_found(&format!("{name}/{}", version.unwrap_or(""))))?;
            found.apply(self.body);
            found.updated = now();
            Ok(self.bundle(&secret_name, found, true, false))
        })
    }

    fn list(&self) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let items: Vec<Value> = vault
                .secrets
                .values()
                .filter_map(|s| Some(self.bundle(&s.name, s.versions.last()?, false, false)))
                .collect();
            Ok(json!({"value": items, "nextLink": null}))
        })
    }

    fn versions(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            // Listing the versions of a missing secret is an empty list, not a 404.
            let items: Vec<Value> = vault
                .secrets
                .get(&name.to_ascii_lowercase())
                .map(|s| s.versions.iter().map(|v| self.bundle(&s.name, v, true, false)).collect())
                .unwrap_or_default();
            Ok(json!({"value": items, "nextLink": null}))
        })
    }

    fn delete(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let key = name.to_ascii_lowercase();
            let secret = vault.secrets.remove(&key).ok_or_else(|| not_found(name))?;
            let deleted = DeletedSecret {
                secret,
                deleted_date: now(),
            };
            let body = self.deleted_bundle(&deleted, false);
            vault.deleted.insert(key, deleted);
            Ok(body)
        })
    }

    fn deleted_not_found(name: &str) -> ArmError {
        ArmError::not_found("SecretNotFound", format!("Deleted Secret not found: {name}"))
    }

    fn get_deleted(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            vault
                .deleted
                .get(&name.to_ascii_lowercase())
                .map(|d| self.deleted_bundle(d, false))
                .ok_or_else(|| Self::deleted_not_found(name))
        })
    }

    fn list_deleted(&self) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let items: Vec<Value> = vault
                .deleted
                .values()
                .map(|d| {
                    let mut item = self.deleted_bundle(d, false);
                    // Deleted items are identified by name, like live list items.
                    item["id"] = json!(format!("{}secrets/{}", self.base(), d.secret.name));
                    item
                })
                .collect();
            Ok(json!({"value": items, "nextLink": null}))
        })
    }

    fn purge(&self, name: &str) -> Result<(), ArmError> {
        self.with_vault(|vault| {
            vault
                .deleted
                .remove(&name.to_ascii_lowercase())
                .map(|_| ())
                .ok_or_else(|| Self::deleted_not_found(name))
        })
    }

    fn recover(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let key = name.to_ascii_lowercase();
            let deleted = vault
                .deleted
                .remove(&key)
                .ok_or_else(|| Self::deleted_not_found(name))?;
            let secret = deleted.secret;
            let body = self.bundle(&secret.name, secret.versions.last().expect("has a version"), true, false);
            vault.secrets.insert(key, secret);
            Ok(body)
        })
    }
}

fn unauthorized(engine: &Engine, message: &str) -> Response {
    let config = engine.config();
    let challenge = format!(
        "Bearer authorization=\"{}/{}\", resource=\"{}\"",
        config.management_endpoint(),
        config.tenant_id,
        config.vault_resource()
    );
    let mut response = ArmError::new(StatusCode::UNAUTHORIZED, "Unauthorized", message).into_response();
    if let Ok(value) = HeaderValue::from_str(&challenge) {
        response.headers_mut().insert(header::WWW_AUTHENTICATE, value);
    }
    response
}

/// What a secrets operation needs: its data action under role-based access
/// control, and its permission under an access policy.
struct Needed {
    /// `secrets` or `keys`: the access policy permission list it is in.
    kind: &'static str,
    action: &'static str,
    /// The access-policy permission, lowercased.
    policy: &'static str,
}

fn permission_for(method: &Method, collection: &str, rest: &[&str]) -> Option<Needed> {
    if matches!(collection, "keys" | "deletedkeys") {
        return key_permission_for(method, collection, rest);
    }
    if collection == "certificates" && rest.first().is_some_and(|r| r.eq_ignore_ascii_case("contacts")) {
        let action = match method.as_str() {
            "GET" => "Microsoft.KeyVault/vaults/certificates/read",
            _ => "Microsoft.KeyVault/vaults/certificatecontacts/write",
        };
        return Some(Needed { kind: "certificates", action, policy: "managecontacts" });
    }
    let needed = |action, policy| Some(Needed { kind: "secrets", action, policy });
    const READ_METADATA: &str = "Microsoft.KeyVault/vaults/secrets/readMetadata/action";
    match (collection, rest, method.as_str()) {
        ("secrets", [], "GET") | ("secrets", [_, "versions"], "GET") => needed(READ_METADATA, "list"),
        ("secrets", [_], "PUT") => needed("Microsoft.KeyVault/vaults/secrets/setSecret/action", "set"),
        ("secrets", [_] | [_, _], "GET") => needed("Microsoft.KeyVault/vaults/secrets/getSecret/action", "get"),
        ("secrets", [_] | [_, _], "PATCH") => needed("Microsoft.KeyVault/vaults/secrets/update/action", "set"),
        ("secrets", [_], "DELETE") => needed("Microsoft.KeyVault/vaults/secrets/delete", "delete"),
        ("deletedsecrets", [] | [_], "GET") => needed(READ_METADATA, "list"),
        ("deletedsecrets", [_], "DELETE") => needed("Microsoft.KeyVault/vaults/secrets/purge/action", "purge"),
        ("deletedsecrets", [_, "recover"], "POST") => needed("Microsoft.KeyVault/vaults/secrets/recover/action", "recover"),
        _ => None,
    }
}

fn key_permission_for(method: &Method, collection: &str, rest: &[&str]) -> Option<Needed> {
    let needed = |action, policy| Some(Needed { kind: "keys", action, policy });
    const READ: &str = "Microsoft.KeyVault/vaults/keys/read";
    let lower: Vec<String> = rest.iter().map(|s| s.to_ascii_lowercase()).collect();
    let lower: Vec<&str> = lower.iter().map(String::as_str).collect();
    let crypto = |op: &str| match op {
        "encrypt" => needed("Microsoft.KeyVault/vaults/keys/encrypt/action", "encrypt"),
        "decrypt" => needed("Microsoft.KeyVault/vaults/keys/decrypt/action", "decrypt"),
        "sign" => needed("Microsoft.KeyVault/vaults/keys/sign/action", "sign"),
        "verify" => needed("Microsoft.KeyVault/vaults/keys/verify/action", "verify"),
        "wrapkey" => needed("Microsoft.KeyVault/vaults/keys/wrap/action", "wrapkey"),
        "unwrapkey" => needed("Microsoft.KeyVault/vaults/keys/unwrap/action", "unwrapkey"),
        _ => None,
    };
    match (collection, lower.as_slice(), method.as_str()) {
        ("keys", [], "GET") | ("keys", [_, "versions"], "GET") => needed(READ, "list"),
        ("keys", [_, "create"], "POST") => needed("Microsoft.KeyVault/vaults/keys/create/action", "create"),
        ("keys", [_, "rotate"], "POST") => needed("Microsoft.KeyVault/vaults/keys/rotate/action", "rotate"),
        ("keys", ["restore"], "POST") => needed("Microsoft.KeyVault/vaults/keys/restore/action", "restore"),
        ("keys", [_, "backup"], "POST") => needed("Microsoft.KeyVault/vaults/keys/backup/action", "backup"),
        ("keys", [_, "rotationpolicy"], "GET") => needed("Microsoft.KeyVault/vaults/keyrotationpolicies/read", "getrotationpolicy"),
        ("keys", [_, "rotationpolicy"], "PUT") => needed("Microsoft.KeyVault/vaults/keyrotationpolicies/write", "setrotationpolicy"),
        ("keys", [_, op], "POST") | ("keys", [_, _, op], "POST") => crypto(op),
        ("keys", [_], "PUT") => needed("Microsoft.KeyVault/vaults/keys/import/action", "import"),
        ("keys", [_] | [_, _], "GET") => needed(READ, "get"),
        ("keys", [_] | [_, _], "PATCH") => needed("Microsoft.KeyVault/vaults/keys/update/action", "update"),
        ("keys", [_], "DELETE") => needed("Microsoft.KeyVault/vaults/keys/delete", "delete"),
        ("deletedkeys", [] | [_], "GET") => needed(READ, "list"),
        ("deletedkeys", [_], "DELETE") => needed("Microsoft.KeyVault/vaults/keys/purge/action", "purge"),
        ("deletedkeys", [_, "recover"], "POST") => needed("Microsoft.KeyVault/vaults/keys/recover/action", "recover"),
        _ => None,
    }
}

/// With role assignments enforced, checks the caller against the vault's
/// permission model: role assignments for a vault with
/// `enableRbacAuthorization`, its access policies otherwise.
fn check_access(engine: &Engine, vault: &str, claims: &auth::Claims, needed: &Needed, secret: Option<&str>) -> Result<(), Response> {
    use emulator_core::arm::{data_access, principal_ids, ArmState, Permission};
    if !engine.config().enforce_rbac {
        return Ok(());
    }
    let (vault_id, location, properties) = {
        let state = engine.state::<ArmState>();
        let state = state.lock();
        let found = state
            .resources()
            .find(|r| r.resource_type.eq_ignore_ascii_case("Microsoft.KeyVault/vaults") && r.name.eq_ignore_ascii_case(vault))
            .map(|r| (r.id.clone(), r.location.clone().unwrap_or_default(), r.properties.clone()));
        found.unwrap_or_default()
    };
    let caller = format!("appid={};oid={};iss={}", claims.appid, claims.oid, claims.iss);
    let forbidden = |message: String, inner: &str| {
        json_response(
            StatusCode::FORBIDDEN,
            &json!({"error": {"code": "Forbidden", "message": message, "innererror": {"code": inner}}}),
        )
    };
    if properties.get("enableRbacAuthorization").and_then(Value::as_bool) == Some(true) {
        let scope = match secret {
            Some(name) => format!("{vault_id}/{}/{name}", needed.kind),
            None => vault_id.clone(),
        };
        if data_access(engine, &claims.oid, &[Permission::Data(needed.action)], &scope) {
            return Ok(());
        }
        return Err(forbidden(
            format!(
                "Caller is not authorized to perform action on resource.\r\nIf role assignments, deny assignments or role definitions were changed recently, please observe propagation time.\r\nCaller: {caller}\r\nAction: '{}'\r\nResource: '{}'\r\nAssignment: (not found)\r\nDenyAssignmentId: null\r\nDecisionReason: null \r\nVault: {vault};location={}\r\n",
                needed.action,
                scope.to_ascii_lowercase(),
                location,
            ),
            "ForbiddenByRbac",
        ));
    }
    let ids = principal_ids(engine, &claims.oid);
    let tenant = &engine.config().tenant_id;
    let allowed = properties
        .get("accessPolicies")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|p| p.get("tenantId").and_then(Value::as_str).is_none_or(|t| t.eq_ignore_ascii_case(tenant)))
        .filter(|p| p.get("objectId").and_then(Value::as_str).is_some_and(|o| ids.iter().any(|id| id.eq_ignore_ascii_case(o))))
        .flat_map(|p| p.pointer(&format!("/permissions/{}", needed.kind)).and_then(Value::as_array).cloned().unwrap_or_default())
        .filter_map(|p| p.as_str().map(str::to_ascii_lowercase))
        .any(|p| p == needed.policy || p == "all");
    if allowed {
        return Ok(());
    }
    Err(forbidden(
        format!(
            "The user, group or application '{caller}' does not have {} {} permission on key vault '{vault};location={}'. For help resolving this issue, please see https://go.microsoft.com/fwlink/?linkid=2125287",
            needed.kind,
            needed.policy,
            location,
        ),
        "AccessDenied",
    ))
}

/// The vault's certificate contacts: get, set and delete.
fn certificate_contacts(engine: &Engine, vault: &str, method: &Method, body: &Value) -> Result<Response, ArmError> {
    let state = engine.state::<KeyVaultState>();
    let mut state = state.lock();
    let data = state.vaults.get_mut(vault).ok_or_else(|| ArmError::not_found("VaultNotFound", "The vault was not found."))?;
    let id = format!("{}certificates/contacts", engine.config().vault_uri(vault));
    let document = |contacts: &Value| json!({"id": id, "contacts": contacts});
    // Unverified against Azure: the code and message when no contacts are set.
    let none = || ArmError::not_found("ContactsNotFound", "Contacts not found");
    let reply = match method.as_str() {
        "GET" => document(data.certificate_contacts.as_ref().ok_or_else(none)?),
        "PUT" => {
            let contacts = body
                .get("contacts")
                .filter(|c| c.is_array())
                .ok_or_else(|| ArmError::bad_request("BadParameter", "The property 'contacts' must be an array."))?;
            data.certificate_contacts = Some(contacts.clone());
            document(contacts)
        }
        "DELETE" => document(&data.certificate_contacts.take().ok_or_else(none)?),
        _ => return Err(ArmError::not_implemented("this Key Vault operation")),
    };
    Ok(json_response(StatusCode::OK, &reply))
}

/// Serves one Key Vault data-plane request addressed to `vault`.
pub async fn handle(engine: Arc<Engine>, vault: String, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let mut response = match serve(&engine, &vault.to_ascii_lowercase(), &parts, body).await {
        Ok(response) => response,
        Err(response) => response,
    };
    if let Ok(value) = HeaderValue::from_str(&request_id()) {
        response.headers_mut().insert("x-ms-request-id", value);
    }
    response
}

async fn serve(
    engine: &Engine,
    vault: &str,
    parts: &http::request::Parts,
    body: Body,
) -> Result<Response, Response> {
    let audiences = [engine.config().vault_resource(), "https://vault.azure.net".to_string()];
    // SDKs send their first request without a token (and without a body) to learn
    // from this challenge which authority and resource to ask a token for.
    let claims = auth::authorize(&parts.headers, &audiences).map_err(|failure| {
        let message = match failure {
            AuthFailure::Missing => "AKV10000: Request is missing a Bearer or PoP token.",
            AuthFailure::Invalid => "AKV10001: Unable to parse JWT token.",
            AuthFailure::WrongAudience => "AKV10022: Invalid audience.",
        };
        unauthorized(engine, message)
    })?;

    let fail = |error: ArmError| error.into_response();
    if !engine.state::<KeyVaultState>().lock().vaults.contains_key(vault) {
        // A real request for a missing vault fails in DNS; this is the nearest answer.
        return Err(fail(ArmError::not_found("VaultNotFound", "The vault was not found.")));
    }
    let query = parse_query(parts.uri.query());
    if !query.contains_key("api-version") {
        return Err(fail(ArmError::bad_request("BadParameter", "api-version must be specified")));
    }
    let bytes = read_body(body).await?;
    let body: Value = if bytes.iter().all(u8::is_ascii_whitespace) {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).map_err(|_| {
            fail(ArmError::bad_request("BadParameter", "The request body is not valid JSON."))
        })?
    };

    let segments: Vec<String> = parts
        .uri
        .path()
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| percent_decode(s, false))
        .collect();
    let collection = segments.first().map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    let rest: Vec<&str> = segments.iter().skip(1).map(String::as_str).collect();
    if let Some(name) = rest.first() {
        if !is_valid_secret_name(name) {
            return Err(fail(ArmError::bad_request(
                "BadParameter",
                format!("The request URI contains an invalid name: {name}"),
            )));
        }
    }

    let method = &parts.method;
    if let Some(needed) = permission_for(method, &collection, &rest) {
        check_access(engine, vault, &claims, &needed, rest.first().copied())?;
    }
    if collection == "certificates" && matches!(rest.as_slice(), [contacts] if contacts.eq_ignore_ascii_case("contacts")) {
        return certificate_contacts(engine, vault, method, &body).map_err(fail);
    }
    if matches!(collection.as_str(), "keys" | "deletedkeys" | "rng") {
        return crate::keys_data::serve(engine, vault, method, &collection, &rest, &body)
            .map(|reply| match reply {
                Some(body) => json_response(StatusCode::OK, &body),
                None => StatusCode::NO_CONTENT.into_response(),
            })
            .map_err(fail);
    }
    let request = SecretRequest {
        engine,
        vault,
        body: &body,
    };
    let result = match (collection.as_str(), rest.as_slice()) {
        ("secrets", []) if method == Method::GET => request.list(),
        ("secrets", [name]) => match method.as_str() {
            "PUT" => request.set(name),
            "GET" => request.get(name, None),
            "PATCH" => request.update(name, None),
            "DELETE" => request.delete(name),
            _ => Err(ArmError::not_implemented("this Key Vault operation")),
        },
        ("secrets", [name, "versions"]) if method == Method::GET => request.versions(name),
        ("secrets", [name, version]) if method == Method::GET => request.get(name, Some(version)),
        ("secrets", [name, version]) if method == Method::PATCH => request.update(name, Some(version)),
        ("deletedsecrets", []) if method == Method::GET => request.list_deleted(),
        ("deletedsecrets", [name]) if method == Method::GET => request.get_deleted(name),
        ("deletedsecrets", [name]) if method == Method::DELETE => {
            return request
                .purge(name)
                .map(|()| StatusCode::NO_CONTENT.into_response())
                .map_err(fail);
        }
        ("deletedsecrets", [name, "recover"]) if method == Method::POST => request.recover(name),
        _ => Err(ArmError::not_implemented("this Key Vault operation")),
    };
    result
        .map(|body| json_response(StatusCode::OK, &body))
        .map_err(fail)
}
