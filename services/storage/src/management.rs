//! Storage resources under an account on Resource Manager: the four service
//! singletons, blob containers, queues, tables, file shares, encryption scopes,
//! and the lifecycle-management and inventory policies.
//!
//! Blob containers are one store seen from both planes: a container created here
//! is usable on the Blob data plane and vice versa. Queues, tables and file shares
//! created here are created on their data planes too.
//!
//! Not checked against Azure: the default service properties, and the code and
//! wording of validation refusals.

use std::sync::Arc;
use std::time::SystemTime;

use emulator_core::arm::{
    ArmContext, ArmRegistry, ArmState, OperationRequest, PutRequest, Resource, ResourceTypeHandler,
};
use emulator_core::http_util::iso_timestamp;
use emulator_core::ArmError;
use http::StatusCode;
use serde_json::{json, Map, Value};

use crate::blob::is_valid_container_name;
use crate::protection::{container_policy_json, legal_hold_json, protected};
use crate::state::Container;
use crate::StorageState;

const ACCOUNT_TYPE: &str = "Microsoft.Storage/storageAccounts";
const CONTAINER_TYPE: &str = "Microsoft.Storage/storageAccounts/blobServices/containers";

/// The service singletons every account has, with their initial properties.
fn services() -> [(&'static str, Value); 4] {
    let cors = json!({"corsRules": []});
    [
        (
            "blobServices",
            json!({"cors": cors, "deleteRetentionPolicy": {"allowPermanentDelete": false, "enabled": false}}),
        ),
        (
            "fileServices",
            json!({"cors": cors, "shareDeleteRetentionPolicy": {"enabled": true, "days": 7}}),
        ),
        ("queueServices", json!({"cors": cors})),
        ("tableServices", json!({"cors": cors})),
    ]
}

pub(crate) fn register(registry: &mut ArmRegistry) {
    for (service, _) in services() {
        registry.register_type(Service(match service {
            "blobServices" => "Microsoft.Storage/storageAccounts/blobServices",
            "fileServices" => "Microsoft.Storage/storageAccounts/fileServices",
            "queueServices" => "Microsoft.Storage/storageAccounts/queueServices",
            _ => "Microsoft.Storage/storageAccounts/tableServices",
        }));
    }
    registry.register_type(Queues);
    registry.register_type(Tables);
    registry.register_type(FileShares);
    registry.register_type(EncryptionScopes);
    registry.register_type(LocalUsers);
    registry.register_type(Policies {
        resource_type: "Microsoft.Storage/storageAccounts/managementPolicies",
    });
    registry.register_type(Policies {
        resource_type: "Microsoft.Storage/storageAccounts/inventoryPolicies",
    });
    registry.register_operation("PrivateLinkResources_ListByStorageAccount", Arc::new(private_link_resources));
    registry.register_operation("BlobContainers_List", Arc::new(list_containers));
    registry.register_operation("BlobContainers_Get", Arc::new(get_container));
    registry.register_operation("BlobContainers_Create", Arc::new(put_container));
    registry.register_operation("BlobContainers_Update", Arc::new(patch_container));
    registry.register_operation("BlobContainers_Delete", Arc::new(delete_container));
    registry.register_operation("StorageAccounts_GetCustomerInitiatedMigration", Arc::new(get_migration));
}

fn invalid(message: impl Into<String>) -> ArmError {
    ArmError::bad_request("BadRequest", message)
}

/// The members of the request's `properties` object.
fn requested(body: &Value) -> Map<String, Value> {
    body.get("properties").and_then(Value::as_object).cloned().unwrap_or_default()
}

/// Creates an account's service singletons. They are part of the account: they
/// exist from the moment it does and cannot be created separately.
pub(crate) fn create_services(ctx: &ArmContext<'_>, account: &Resource) {
    let state = ctx.engine.state::<ArmState>();
    let mut state = state.lock();
    for (service, properties) in services() {
        state.insert(Resource {
            id: format!("{}/{service}/default", account.id),
            name: "default".into(),
            resource_type: format!("{ACCOUNT_TYPE}/{service}"),
            location: None,
            tags: None,
            kind: None,
            sku: None,
            identity: None,
            properties,
        });
    }
}

/// One of the four per-account service singletons, named by its type.
struct Service(&'static str);

impl ResourceTypeHandler for Service {
    fn resource_type(&self) -> &'static str {
        self.0
    }

    fn put(&self, _ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        // Only the singleton the account came with can be set.
        let existing = request.existing.ok_or_else(|| {
            ArmError::not_found(
                "ResourceNotFound",
                format!("The service '{}' does not exist; the only service name is 'default'.", request.name),
            )
        })?;
        let mut properties = existing.properties.clone();
        for (member, value) in requested(request.body) {
            properties[member] = value;
        }
        Ok(properties)
    }

    fn patch(&self, ctx: &ArmContext<'_>, existing: &Resource, body: &Value) -> Result<Value, ArmError> {
        patch_as_put(self, ctx, existing, body)
    }

    fn view(&self, ctx: &ArmContext<'_>, resource: &mut Resource) {
        // The service reports its account's SKU.
        let account_id = resource.id.rsplitn(3, '/').nth(2).unwrap_or("").to_string();
        let state = ctx.engine.state::<ArmState>();
        resource.sku = state.lock().resource(&account_id).and_then(|account| account.sku.clone());
    }
}

/// Serves a `PATCH` with the type's own `PUT` logic over the stored resource, so an
/// update is validated the same way and cannot set service-owned properties.
fn patch_as_put(
    handler: &dyn ResourceTypeHandler,
    ctx: &ArmContext<'_>,
    existing: &Resource,
    body: &Value,
) -> Result<Value, ArmError> {
    handler.put(
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

/// Stored properties (or `defaults`) overlaid with the `settable` request members.
fn overlay(request: &PutRequest<'_>, defaults: Value, settable: &[&str]) -> Value {
    let mut properties = request.existing.map(|r| r.properties.clone()).unwrap_or(defaults);
    let given = requested(request.body);
    for member in settable {
        if let Some(value) = given.get(*member).filter(|v| !v.is_null()) {
            properties[*member] = value.clone();
        }
    }
    properties
}

struct Queues;

impl ResourceTypeHandler for Queues {
    fn resource_type(&self) -> &'static str {
        "Microsoft.Storage/storageAccounts/queueServices/queues"
    }

    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        // Queue names follow the same rule as container names.
        if !is_valid_container_name(request.name) {
            return Err(invalid(format!(
                "The queue name '{}' is invalid. It must be 3 to 63 characters of lowercase letters, numbers and single hyphens, and start and end with a letter or number.",
                request.name
            )));
        }
        let properties = overlay(request, json!({"metadata": {}, "approximateMessageCount": 0}), &["metadata"]);
        // The queue the Queue data plane serves, with this metadata.
        let account = request.id.split('/').skip_while(|p| !p.eq_ignore_ascii_case("storageAccounts")).nth(1).unwrap_or_default().to_string();
        let metadata: std::collections::BTreeMap<String, String> = properties
            .get("metadata")
            .and_then(Value::as_object)
            .map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
            .unwrap_or_default();
        let state = ctx.engine.state::<StorageState>();
        let mut state = state.lock();
        if let Some(account) = state.accounts.get_mut(&account) {
            account.queues.entry(request.name.to_string()).or_default().metadata = metadata;
        }
        Ok(properties)
    }

    fn patch(&self, ctx: &ArmContext<'_>, existing: &Resource, body: &Value) -> Result<Value, ArmError> {
        patch_as_put(self, ctx, existing, body)
    }

    fn view(&self, ctx: &ArmContext<'_>, resource: &mut Resource) {
        let account = resource.id.split('/').skip_while(|p| !p.eq_ignore_ascii_case("storageAccounts")).nth(1).unwrap_or_default().to_string();
        let state = ctx.engine.state::<StorageState>();
        let mut state = state.lock();
        if let Some(queue) = state.accounts.get_mut(&account).and_then(|a| a.queues.get_mut(&resource.name)) {
            resource.properties["approximateMessageCount"] = json!(queue.count());
        }
    }

    /// The specification answers a queue delete with 204 only.
    fn delete_status(&self) -> http::StatusCode {
        http::StatusCode::NO_CONTENT
    }

    fn deleted(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        let account = resource.id.split('/').skip_while(|p| !p.eq_ignore_ascii_case("storageAccounts")).nth(1).unwrap_or_default().to_string();
        let state = ctx.engine.state::<StorageState>();
        let mut state = state.lock();
        if let Some(account) = state.accounts.get_mut(&account) {
            account.queues.remove(&resource.name);
        }
    }
}

struct Tables;

impl ResourceTypeHandler for Tables {
    fn resource_type(&self) -> &'static str {
        "Microsoft.Storage/storageAccounts/tableServices/tables"
    }

    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        // The specification's pattern: ^[A-Za-z][A-Za-z0-9]{2,62}$
        let name = request.name.as_bytes();
        let valid = (3..=63).contains(&name.len())
            && name[0].is_ascii_alphabetic()
            && name.iter().all(u8::is_ascii_alphanumeric);
        if !valid {
            return Err(invalid(format!(
                "The table name '{}' is invalid. It must be 3 to 63 letters and numbers and start with a letter.",
                request.name
            )));
        }
        let account = request.id.split('/').skip_while(|p| !p.eq_ignore_ascii_case("storageAccounts")).nth(1).unwrap_or_default().to_string();
        let state = ctx.engine.state::<StorageState>();
        let mut state = state.lock();
        if let Some(account) = state.accounts.get_mut(&account) {
            account.tables.entry(request.name.to_ascii_lowercase()).or_insert_with(|| crate::table::Table::new(request.name));
        }
        Ok(overlay(request, json!({"tableName": request.name}), &["signedIdentifiers"]))
    }

    /// The specification answers a table delete with 204 only.
    fn delete_status(&self) -> http::StatusCode {
        http::StatusCode::NO_CONTENT
    }

    fn deleted(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        let account = resource.id.split('/').skip_while(|p| !p.eq_ignore_ascii_case("storageAccounts")).nth(1).unwrap_or_default().to_string();
        let state = ctx.engine.state::<StorageState>();
        let mut state = state.lock();
        if let Some(account) = state.accounts.get_mut(&account) {
            account.tables.remove(&resource.name.to_ascii_lowercase());
        }
    }

    fn patch(&self, ctx: &ArmContext<'_>, existing: &Resource, body: &Value) -> Result<Value, ArmError> {
        patch_as_put(self, ctx, existing, body)
    }
}

struct FileShares;

impl ResourceTypeHandler for FileShares {
    fn resource_type(&self) -> &'static str {
        "Microsoft.Storage/storageAccounts/fileServices/shares"
    }

    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        if !is_valid_container_name(request.name) {
            return Err(invalid(format!(
                "The share name '{}' is invalid. It must be 3 to 63 characters of lowercase letters, numbers and single hyphens, and start and end with a letter or number.",
                request.name
            )));
        }
        let given = requested(request.body);
        if let Some(quota) = given.get("shareQuota").filter(|q| !q.is_null()) {
            if !quota.as_i64().is_some_and(|gib| (1..=102_400).contains(&gib)) {
                return Err(invalid("The share quota must be between 1 and 102400 gibibytes."));
            }
        }
        if request.existing.is_some()
            && given
                .get("enabledProtocols")
                .is_some_and(|protocols| Some(protocols) != request.existing.and_then(|r| r.properties.get("enabledProtocols")))
        {
            return Err(invalid("The enabled protocols of a share cannot be changed after it is created."));
        }
        let mut properties = overlay(
            request,
            json!({
                "shareQuota": 5120,
                "enabledProtocols": "SMB",
                "accessTier": "TransactionOptimized",
                "leaseStatus": "unlocked",
                "leaseState": "available",
            }),
            &["shareQuota", "enabledProtocols", "accessTier", "rootSquash", "metadata", "signedIdentifiers"],
        );
        let now = iso_timestamp();
        if given.contains_key("accessTier") || request.existing.is_none() {
            properties["accessTierChangeTime"] = json!(now);
        }
        properties["lastModifiedTime"] = json!(now);
        // The share on the Files data plane: made with the resource, and
        // given its quota.
        let quota = properties.get("shareQuota").and_then(Value::as_u64).unwrap_or(5120);
        let account = request.id.split('/').skip_while(|p| !p.eq_ignore_ascii_case("storageAccounts")).nth(1).unwrap_or_default().to_string();
        let state = ctx.engine.state::<StorageState>();
        let mut state = state.lock();
        let etag = state.next_etag();
        if let Some(account) = state.accounts.get_mut(&account) {
            account.shares.entry(request.name.to_string()).or_insert_with(|| crate::files::Share::new(quota, etag)).quota = quota;
        }
        Ok(properties)
    }

    fn patch(&self, ctx: &ArmContext<'_>, existing: &Resource, body: &Value) -> Result<Value, ArmError> {
        patch_as_put(self, ctx, existing, body)
    }

    fn actions(&self) -> &'static [&'static str] {
        &["lease"]
    }

    fn action(&self, ctx: &ArmContext<'_>, resource: &Resource, action: &str, body: &Value) -> Result<Value, ArmError> {
        if !action.eq_ignore_ascii_case("lease") {
            return Err(ArmError::not_implemented(format_args!("the '{action}' action on {}", self.resource_type())));
        }
        let state = ctx.engine.state::<StorageState>();
        let mut state = state.lock();
        let key = resource.id.to_ascii_lowercase();
        let mut slot = state.share_leases.remove(&key);
        let reply = crate::protection::apply_lease_action(&mut slot, body, "share");
        if let Some(lease) = slot {
            state.share_leases.insert(key, lease);
        }
        reply
    }

    fn view(&self, ctx: &ArmContext<'_>, resource: &mut Resource) {
        let state = ctx.engine.state::<StorageState>();
        let state = state.lock();
        let lease = state.share_leases.get(&resource.id.to_ascii_lowercase());
        let current = crate::state::lease_state(lease, SystemTime::now());
        resource.properties["leaseState"] = json!(current.as_str().to_ascii_lowercase());
        resource.properties["leaseStatus"] = json!(if current.blocks() { "locked" } else { "unlocked" });
        match lease.filter(|_| current.blocks()) {
            Some(lease) => resource.properties["leaseDuration"] = json!(if lease.duration.is_none() { "infinite" } else { "fixed" }),
            None => {
                if let Some(properties) = resource.properties.as_object_mut() {
                    properties.remove("leaseDuration");
                }
            }
        }
    }

    fn before_delete(&self, ctx: &ArmContext<'_>, resource: &Resource) -> Result<(), ArmError> {
        let state = ctx.engine.state::<StorageState>();
        let state = state.lock();
        let lease = state.share_leases.get(&resource.id.to_ascii_lowercase());
        if crate::state::lease_state(lease, SystemTime::now()).blocks() {
            return Err(ArmError::new(
                StatusCode::PRECONDITION_FAILED,
                "LeaseIdMissing",
                "There is currently a lease on the file share and no lease ID was specified in the request.",
            ));
        }
        Ok(())
    }

    fn deleted(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        let state = ctx.engine.state::<StorageState>();
        let mut state = state.lock();
        state.share_leases.remove(&resource.id.to_ascii_lowercase());
        let account = resource.id.split('/').skip_while(|p| !p.eq_ignore_ascii_case("storageAccounts")).nth(1).unwrap_or_default().to_string();
        if let Some(account) = state.accounts.get_mut(&account) {
            account.shares.remove(&resource.name);
        }
    }
}

struct EncryptionScopes;

impl ResourceTypeHandler for EncryptionScopes {
    fn resource_type(&self) -> &'static str {
        "Microsoft.Storage/storageAccounts/encryptionScopes"
    }

    fn put(&self, _ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        let given = requested(request.body);
        for (member, allowed) in [
            ("source", ["Microsoft.Storage", "Microsoft.KeyVault"]),
            ("state", ["Enabled", "Disabled"]),
        ] {
            if let Some(value) = given.get(member).filter(|v| !v.is_null()) {
                if !value.as_str().is_some_and(|v| allowed.contains(&v)) {
                    return Err(invalid(format!(
                        "The value of 'properties.{member}' must be {} or {}.",
                        allowed[0], allowed[1]
                    )));
                }
            }
        }
        let mut properties = overlay(
            request,
            json!({"source": "Microsoft.Storage", "state": "Enabled", "creationTime": iso_timestamp()}),
            &["source", "state", "keyVaultProperties"],
        );
        // Infrastructure encryption is chosen once, at creation.
        if request.existing.is_none() {
            if let Some(value) = given.get("requireInfrastructureEncryption").filter(|v| !v.is_null()) {
                properties["requireInfrastructureEncryption"] = value.clone();
            }
        }
        if properties["source"] == "Microsoft.KeyVault" && properties.pointer("/keyVaultProperties/keyUri").is_none() {
            return Err(invalid(
                "An encryption scope whose source is Microsoft.KeyVault requires 'properties.keyVaultProperties.keyUri'.",
            ));
        }
        properties["lastModifiedTime"] = json!(iso_timestamp());
        Ok(properties)
    }

    fn patch(&self, ctx: &ArmContext<'_>, existing: &Resource, body: &Value) -> Result<Value, ArmError> {
        patch_as_put(self, ctx, existing, body)
    }
}

/// Local users: the SFTP and NFS identities of an account.
struct LocalUsers;

/// A random string of `len` characters from `alphabet`.
fn random_text(len: usize, alphabet: &[u8]) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..len).map(|_| alphabet[rng.gen_range(0..alphabet.len())] as char).collect()
}

impl ResourceTypeHandler for LocalUsers {
    fn resource_type(&self) -> &'static str {
        "Microsoft.Storage/storageAccounts/localUsers"
    }

    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        // The specification: 3 to 64 lowercase letters and numbers.
        let name = request.name.as_bytes();
        let valid = (3..=64).contains(&name.len()) && name.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        if !valid {
            return Err(invalid(format!(
                "The local user name '{}' is invalid. It must be 3 to 64 lowercase letters and numbers.",
                request.name
            )));
        }
        let defaults = match request.existing {
            Some(_) => json!({}),
            None => {
                let state = ctx.engine.state::<StorageState>();
                let mut state = state.lock();
                state.local_user_count += 1;
                // The service assigns these once.
                json!({
                    "sid": format!("S-1-2-0-{}-{}-{}-{}", random_text(10, b"123456789"), random_text(10, b"123456789"), random_text(10, b"123456789"), 999 + state.local_user_count),
                    "userId": 999 + state.local_user_count,
                    "hasSharedKey": false,
                    "hasSshKey": false,
                    "hasSshPassword": false,
                })
            }
        };
        let mut properties = overlay(
            request,
            defaults,
            &[
                "permissionScopes",
                "homeDirectory",
                "sshAuthorizedKeys",
                "hasSharedKey",
                "hasSshKey",
                "hasSshPassword",
                "groupId",
                "allowAclAuthorization",
                "extendedGroups",
                "isNFSv3Enabled",
            ],
        );
        // Supplying keys is what gives a user an SSH key.
        if requested(request.body).get("sshAuthorizedKeys").is_some_and(|keys| keys.as_array().is_some_and(|k| !k.is_empty())) {
            properties["hasSshKey"] = json!(true);
        }
        Ok(properties)
    }

    fn patch(&self, ctx: &ArmContext<'_>, existing: &Resource, body: &Value) -> Result<Value, ArmError> {
        patch_as_put(self, ctx, existing, body)
    }

    fn view(&self, _ctx: &ArmContext<'_>, resource: &mut Resource) {
        // Authorized keys are only ever returned by listKeys.
        if let Some(properties) = resource.properties.as_object_mut() {
            properties.remove("sshAuthorizedKeys");
        }
    }

    fn actions(&self) -> &'static [&'static str] {
        &["listKeys", "regeneratePassword"]
    }

    fn action(&self, ctx: &ArmContext<'_>, resource: &Resource, action: &str, _body: &Value) -> Result<Value, ArmError> {
        match action.to_ascii_lowercase().as_str() {
            "listkeys" => {
                let mut out = json!({
                    "sshAuthorizedKeys": resource.properties.get("sshAuthorizedKeys").cloned().unwrap_or_else(|| json!([])),
                });
                if resource.properties["hasSharedKey"] == true {
                    let state = ctx.engine.state::<StorageState>();
                    let mut state = state.lock();
                    let key = state
                        .local_user_keys
                        .entry(resource.id.to_ascii_lowercase())
                        .or_insert_with(crate::new_key);
                    out["sharedKey"] = json!(key);
                }
                Ok(out)
            }
            "regeneratepassword" => {
                // The password is shown once and never stored in readable form.
                let password = random_text(32, b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789");
                let state = ctx.engine.state::<ArmState>();
                if let Some(user) = state.lock().resource_mut(&resource.id) {
                    user.properties["hasSshPassword"] = json!(true);
                }
                Ok(json!({"sshPassword": password}))
            }
            other => Err(ArmError::not_implemented(format_args!(
                "the '{other}' action on {}",
                self.resource_type()
            ))),
        }
    }

    fn deleted(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        ctx.engine
            .state::<StorageState>()
            .lock()
            .local_user_keys
            .remove(&resource.id.to_ascii_lowercase());
    }
}

/// The lifecycle-management and blob-inventory policies: one per account, always
/// named `default`, holding a `policy` document.
struct Policies {
    resource_type: &'static str,
}

impl ResourceTypeHandler for Policies {
    fn resource_type(&self) -> &'static str {
        self.resource_type
    }

    fn put(&self, _ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        if request.name != "default" {
            return Err(invalid(format!(
                "The policy name '{}' is invalid. The only supported policy name is 'default'.",
                request.name
            )));
        }
        let policy = request
            .body
            .pointer("/properties/policy")
            .filter(|policy| policy.is_object())
            .ok_or_else(|| invalid("The request content was invalid: the required property 'properties.policy' is missing."))?;
        if !policy.get("rules").is_some_and(Value::is_array) {
            return Err(invalid("The request content was invalid: the required property 'properties.policy.rules' is missing."));
        }
        Ok(json!({"policy": policy, "lastModifiedTime": iso_timestamp()}))
    }
}

// ---- blob containers, as Resource Manager sees them -----------------------------

type Reply = Result<(StatusCode, Value), ArmError>;

fn account_id(ctx: &ArmContext<'_>, name: &str) -> String {
    format!(
        "/subscriptions/{}/resourceGroups/{}/providers/{ACCOUNT_TYPE}/{name}",
        ctx.subscription_id, ctx.resource_group
    )
}

/// The account resource's canonical id, or the error for a missing account.
pub(crate) fn require_account(ctx: &ArmContext<'_>, name: &str) -> Result<String, ArmError> {
    let state = ctx.engine.state::<ArmState>();
    let state = state.lock();
    state.resource(&account_id(ctx, name)).map(|account| account.id.clone()).ok_or_else(|| {
        ArmError::not_found(
            "ResourceNotFound",
            format!(
                "The Resource '{ACCOUNT_TYPE}/{name}' under resource group '{}' was not found. For more details please go to https://aka.ms/ARMResourceNotFoundFix",
                ctx.resource_group
            ),
        )
    })
}

/// The sub-resources of an account a private endpoint can target: one per
/// service, plus the secondary endpoints of a read-access geo-redundant account.
/// Get Customer Initiated Migration. The emulator never migrates an account, so
/// the one migration (`default`) targets the account's own SKU and has no
/// status. Clients read this for every account (Terraform's azurerm provider
/// refuses a 404). Unverified against Azure: the body for an account that was
/// never migrated; the specification only gives examples of migrations.
fn get_migration(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let id = require_account(ctx, request.param("accountName"))?;
    let name = request.param("migrationName");
    if name != "default" {
        return Err(ArmError::not_found("ResourceNotFound", format!("The migration '{name}' was not found.")));
    }
    let sku = {
        let state = ctx.engine.state::<ArmState>();
        let state = state.lock();
        let sku = state
            .resource(&id)
            .and_then(|account| account.sku.as_ref())
            .and_then(|sku| sku.get("name"))
            .cloned()
            .unwrap_or(Value::Null);
        sku
    };
    Ok((
        StatusCode::OK,
        json!({
            "id": format!("{id}/accountMigrations/default"),
            "name": "default",
            "type": "Microsoft.Storage/storageAccounts/accountMigrations",
            "properties": {"targetSkuName": sku},
        }),
    ))
}

fn private_link_resources(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let id = require_account(ctx, request.param("accountName"))?;
    let has_secondary = {
        let state = ctx.engine.state::<ArmState>();
        let state = state.lock();
        state
            .resource(&id)
            .and_then(|account| account.sku.as_ref())
            .and_then(|sku| sku.get("name"))
            .and_then(Value::as_str)
            .is_some_and(|sku| sku.contains("RAG"))
    };
    let mut items = Vec::new();
    for service in ["blob", "table", "queue", "file", "web", "dfs"] {
        let suffixes: &[&str] = if has_secondary { &["", "_secondary"] } else { &[""] };
        for suffix in suffixes {
            let group = format!("{service}{suffix}");
            items.push(json!({
                "id": format!("{id}/privateLinkResources/{group}"),
                "name": group,
                "type": "Microsoft.Storage/storageAccounts/privateLinkResources",
                "properties": {
                    "groupId": group,
                    "requiredMembers": [group],
                    "requiredZoneNames": [format!("privatelink.{service}.core.windows.net")],
                },
            }));
        }
    }
    Ok((StatusCode::OK, json!({"value": items})))
}

pub(crate) fn container_not_found() -> ArmError {
    ArmError::not_found("ContainerNotFound", "The specified container does not exist.")
}

fn container_json(account_id: &str, name: &str, container: &Container) -> Value {
    let modified: chrono::DateTime<chrono::Utc> = container.last_modified.into();
    let lease = container.lease_state(SystemTime::now());
    let metadata: Map<String, Value> = container
        .metadata
        .iter()
        .map(|(key, value)| (key.clone(), json!(value)))
        .collect();
    let mut out = json!({
        "id": format!("{account_id}/blobServices/default/containers/{name}"),
        "name": name,
        "type": CONTAINER_TYPE,
        "etag": container.etag,
        "properties": {
            "publicAccess": container.public_access,
            "metadata": metadata,
            // Seven fractional digits, as the service renders timestamps.
            "lastModifiedTime": format!("{}0Z", modified.format("%Y-%m-%dT%H:%M:%S%.6f")),
            "leaseStatus": if lease.blocks() { "Locked" } else { "Unlocked" },
            "leaseState": lease.as_str(),
            "hasImmutabilityPolicy": container.immutability.is_some(),
            "hasLegalHold": !container.legal_hold.is_empty(),
            "legalHold": legal_hold_json(container),
            "deleted": false,
            "remainingRetentionDays": 0,
            "defaultEncryptionScope": container.default_encryption_scope,
            "denyEncryptionScopeOverride": container.deny_encryption_scope_override,
        },
    });
    if lease.blocks() {
        let infinite = container.lease.as_ref().is_some_and(|l| l.duration.is_none());
        out["properties"]["leaseDuration"] = json!(if infinite { "Infinite" } else { "Fixed" });
    }
    if let Some(policy) = &container.immutability {
        out["properties"]["immutabilityPolicy"] = container_policy_json(policy);
    }
    out
}

/// Applies the settable members of a request to a container.
fn apply(container: &mut Container, body: &Value, is_new: bool) -> Result<(), ArmError> {
    let given = requested(body);
    if let Some(access) = given.get("publicAccess").filter(|v| !v.is_null()) {
        match access.as_str() {
            Some(level @ ("None" | "Blob" | "Container")) => container.public_access = level.to_string(),
            _ => return Err(invalid("The value of 'properties.publicAccess' must be None, Blob or Container.")),
        }
    }
    if let Some(metadata) = given.get("metadata").and_then(Value::as_object) {
        container.metadata = metadata
            .iter()
            .map(|(key, value)| (key.clone(), value.as_str().unwrap_or("").to_string()))
            .collect();
    }
    // Encryption scope settings are chosen once, at creation.
    if is_new {
        if let Some(scope) = given.get("defaultEncryptionScope").and_then(Value::as_str) {
            container.default_encryption_scope = scope.to_string();
        }
        if let Some(deny) = given.get("denyEncryptionScopeOverride").and_then(Value::as_bool) {
            container.deny_encryption_scope_override = deny;
        }
    }
    Ok(())
}

fn list_containers(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let account = request.param("accountName");
    let id = require_account(ctx, account)?;
    let state = ctx.engine.state::<StorageState>();
    let state = state.lock();
    let items: Vec<Value> = state
        .accounts
        .get(account)
        .into_iter()
        .flat_map(|a| a.containers.iter())
        .map(|(name, container)| container_json(&id, name, container))
        .collect();
    Ok((StatusCode::OK, json!({"value": items})))
}

fn get_container(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let account = request.param("accountName");
    let name = request.param("containerName");
    let id = require_account(ctx, account)?;
    let state = ctx.engine.state::<StorageState>();
    let state = state.lock();
    let container = state
        .accounts
        .get(account)
        .and_then(|a| a.containers.get(name))
        .ok_or_else(container_not_found)?;
    Ok((StatusCode::OK, container_json(&id, name, container)))
}

/// Creates the container, or updates it if it exists (`create` allows creation).
fn write_container(ctx: &ArmContext<'_>, request: &OperationRequest<'_>, create: bool) -> Reply {
    let account = request.param("accountName");
    let name = request.param("containerName");
    let id = require_account(ctx, account)?;
    if !is_valid_container_name(name) {
        return Err(invalid(format!(
            "The container name '{name}' is invalid. It must be 3 to 63 characters of lowercase letters, numbers and single hyphens, and start and end with a letter or number."
        )));
    }
    let state = ctx.engine.state::<StorageState>();
    let mut state = state.lock();
    let etag = state.next_etag();
    let containers = &mut state.accounts.get_mut(account).ok_or_else(container_not_found)?.containers;
    let is_new = !containers.contains_key(name);
    if is_new && !create {
        return Err(container_not_found());
    }
    let now = SystemTime::now();
    let mut container = containers
        .remove(name)
        .unwrap_or_else(|| Container::new(etag.clone(), now, Vec::new()));
    let result = apply(&mut container, request.body, is_new);
    if result.is_ok() {
        container.etag = etag;
        container.last_modified = now;
    }
    // A refused update leaves the stored container exactly as it was.
    if !is_new || result.is_ok() {
        containers.insert(name.to_string(), container);
    }
    result?;
    let status = if is_new { StatusCode::CREATED } else { StatusCode::OK };
    Ok((status, container_json(&id, name, &containers[name])))
}

fn put_container(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    write_container(ctx, request, true)
}

fn patch_container(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    write_container(ctx, request, false)
}

fn delete_container(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let account = request.param("accountName");
    require_account(ctx, account)?;
    let state = ctx.engine.state::<StorageState>();
    let mut state = state.lock();
    let containers = &mut state.accounts.get_mut(account).ok_or_else(container_not_found)?.containers;
    let name = request.param("containerName");
    if let Some(container) = containers.get(name) {
        let now = SystemTime::now();
        if let Some(protection) = container.deletion_blocker(now) {
            let (code, message) = protected(protection);
            return Err(ArmError::conflict(code, message));
        }
        if container.lease_state(now).blocks() {
            return Err(ArmError::new(
                StatusCode::PRECONDITION_FAILED,
                "LeaseIdMissing",
                "There is currently a lease on the container and no lease ID was specified in the request.",
            ));
        }
    }
    let removed = containers.remove(name);
    // Deleting a container that is not there is not an error on this plane.
    let status = if removed.is_some() { StatusCode::OK } else { StatusCode::NO_CONTENT };
    Ok((status, Value::Null))
}
