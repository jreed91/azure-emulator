//! Cosmos DB accounts on Resource Manager, and their SQL databases and
//! containers (the same store the data plane serves).

use emulator_core::arm::{ArmContext, PutRequest, Resource, ResourceTypeHandler};
use emulator_core::ArmError;
use serde_json::{json, Map, Value};

use crate::store::{self, Account, Container, CosmosState, Database};

pub(crate) const ACCOUNT_TYPE: &str = "Microsoft.DocumentDB/databaseAccounts";
pub(crate) const DATABASE_TYPE: &str = "Microsoft.DocumentDB/databaseAccounts/sqlDatabases";
pub(crate) const CONTAINER_TYPE: &str = "Microsoft.DocumentDB/databaseAccounts/sqlDatabases/containers";

fn invalid(message: impl Into<String>) -> ArmError {
    ArmError::bad_request("BadRequest", message.into())
}

fn is_valid_account_name(name: &str) -> bool {
    (3..=44).contains(&name.len())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// The document endpoint of an account: the emulator's HTTPS port under the
/// account's own hostname.
pub(crate) fn document_endpoint(ctx: &ArmContext<'_>, account: &str) -> String {
    let config = ctx.engine.config();
    format!("{}://{account}.documents.{}:{}/", config.scheme, config.dns_suffix, config.port)
}

/// The name that follows `segment` in a resource id.
fn id_part<'a>(id: &'a str, segment: &str) -> Option<&'a str> {
    let mut parts = id.split('/');
    parts.find(|part| part.eq_ignore_ascii_case(segment))?;
    parts.next()
}

pub(crate) struct DatabaseAccounts;

impl DatabaseAccounts {
    fn keys(ctx: &ArmContext<'_>, resource: &Resource) -> Result<[String; 4], ArmError> {
        let state = ctx.engine.state::<CosmosState>();
        let state = state.lock();
        state
            .accounts
            .get(&resource.name.to_ascii_lowercase())
            .map(|a| a.keys.clone())
            .ok_or_else(|| ArmError::not_found("ResourceNotFound", format!("The account '{}' was not found.", resource.name)))
    }
}

impl ResourceTypeHandler for DatabaseAccounts {
    fn resource_type(&self) -> &'static str {
        ACCOUNT_TYPE
    }

    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        let name = request.name;
        if !is_valid_account_name(name) {
            return Err(invalid(
                "Database account name must be 3 to 44 characters long, contain only lowercase letters, numbers and hyphens, and not start or end with a hyphen.",
            ));
        }
        let given = request.body.get("properties").cloned().unwrap_or_else(|| json!({}));
        let mut properties = match request.existing {
            Some(existing) => existing.properties.clone(),
            None => {
                let state = ctx.engine.state::<CosmosState>();
                let mut state = state.lock();
                let key = name.to_ascii_lowercase();
                if state.accounts.contains_key(&key) {
                    return Err(ArmError::conflict("Conflict", format!("Database account name '{name}' already exists.")));
                }
                state.accounts.insert(key, Account::new());
                let endpoint = document_endpoint(ctx, name);
                let location = request.location.unwrap_or("eastus");
                let region = json!({
                    "id": format!("{name}-{location}"),
                    "locationName": location,
                    "documentEndpoint": endpoint,
                    "provisioningState": "Succeeded",
                    "failoverPriority": 0,
                    "isZoneRedundant": false,
                });
                json!({
                    "provisioningState": "Succeeded",
                    "documentEndpoint": endpoint,
                    "databaseAccountOfferType": "Standard",
                    "consistencyPolicy": {"defaultConsistencyLevel": "Session", "maxIntervalInSeconds": 5, "maxStalenessPrefix": 100},
                    "locations": [region.clone()],
                    "writeLocations": [region.clone()],
                    "readLocations": [region],
                    "failoverPolicies": [{"id": format!("{name}-{location}"), "locationName": location, "failoverPriority": 0}],
                    "capabilities": [],
                    "enableAutomaticFailover": false,
                    "enableMultipleWriteLocations": false,
                    "enableFreeTier": false,
                    "enableAnalyticalStorage": false,
                    "isVirtualNetworkFilterEnabled": false,
                    "virtualNetworkRules": [],
                    "ipRules": [],
                    "publicNetworkAccess": "Enabled",
                    "disableLocalAuth": false,
                    "disableKeyBasedMetadataWriteAccess": false,
                    "minimalTlsVersion": "Tls12",
                    "backupPolicy": {"type": "Periodic", "periodicModeProperties": {"backupIntervalInMinutes": 240, "backupRetentionIntervalInHours": 8, "backupStorageRedundancy": "Geo"}},
                    "instanceId": uuid::Uuid::new_v4().to_string(),
                    "createMode": "Default",
                })
            }
        };
        for member in [
            "consistencyPolicy", "capabilities", "enableAutomaticFailover", "enableFreeTier", "enableAnalyticalStorage",
            "isVirtualNetworkFilterEnabled", "virtualNetworkRules", "ipRules", "publicNetworkAccess", "disableLocalAuth",
            "disableKeyBasedMetadataWriteAccess", "minimalTlsVersion", "backupPolicy", "enableMultipleWriteLocations", "capacity",
        ] {
            if let Some(value) = given.get(member).filter(|v| !v.is_null()) {
                properties[member] = value.clone();
            }
        }
        if let Some(level) = properties.pointer("/consistencyPolicy/defaultConsistencyLevel").and_then(Value::as_str) {
            if !["Eventual", "Session", "BoundedStaleness", "Strong", "ConsistentPrefix"].contains(&level) {
                return Err(invalid(format!("'{level}' is not a consistency level.")));
            }
        }
        Ok(properties)
    }

    fn patch(&self, ctx: &ArmContext<'_>, existing: &Resource, body: &Value) -> Result<Value, ArmError> {
        self.put(ctx, &PutRequest { id: &existing.id, name: &existing.name, location: None, existing: Some(existing), body })
    }

    fn prepare(&self, _ctx: &ArmContext<'_>, resource: &mut Resource) {
        if resource.kind.is_none() {
            resource.kind = Some("GlobalDocumentDB".into());
        }
    }

    fn delete_status(&self) -> http::StatusCode {
        // The specification allows only 202 and 204.
        http::StatusCode::NO_CONTENT
    }

    fn deleted(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        ctx.engine.state::<CosmosState>().lock().accounts.remove(&resource.name.to_ascii_lowercase());
    }

    fn actions(&self) -> &'static [&'static str] {
        &["listKeys", "readonlykeys", "listConnectionStrings", "regenerateKey"]
    }

    fn action(&self, ctx: &ArmContext<'_>, resource: &Resource, action: &str, body: &Value) -> Result<Value, ArmError> {
        match action {
            "listKeys" => {
                let keys = Self::keys(ctx, resource)?;
                Ok(json!({
                    "primaryMasterKey": keys[0],
                    "secondaryMasterKey": keys[1],
                    "primaryReadonlyMasterKey": keys[2],
                    "secondaryReadonlyMasterKey": keys[3],
                }))
            }
            "readonlykeys" => {
                let keys = Self::keys(ctx, resource)?;
                Ok(json!({"primaryReadonlyMasterKey": keys[2], "secondaryReadonlyMasterKey": keys[3]}))
            }
            "listConnectionStrings" => {
                let keys = Self::keys(ctx, resource)?;
                let endpoint = document_endpoint(ctx, &resource.name);
                let entry = |key: &str, description: &str, kind: &str| {
                    json!({
                        "connectionString": format!("AccountEndpoint={endpoint};AccountKey={key};"),
                        "description": description,
                        "keyKind": kind,
                        "type": "Sql",
                    })
                };
                Ok(json!({"connectionStrings": [
                    entry(&keys[0], "Primary SQL Connection String", "Primary"),
                    entry(&keys[1], "Secondary SQL Connection String", "Secondary"),
                    entry(&keys[2], "Primary Read-Only SQL Connection String", "PrimaryReadonly"),
                    entry(&keys[3], "Secondary Read-Only SQL Connection String", "SecondaryReadonly"),
                ]}))
            }
            "regenerateKey" => {
                let index = match body.get("keyKind").and_then(Value::as_str).map(str::to_ascii_lowercase).as_deref() {
                    Some("primary") => 0,
                    Some("secondary") => 1,
                    Some("primaryreadonly") => 2,
                    Some("secondaryreadonly") => 3,
                    _ => return Err(invalid("The keyKind must be primary, secondary, primaryReadonly or secondaryReadonly.")),
                };
                // The emulator also takes the key to set (not an Azure field):
                // a fixed key lets connection strings be written ahead of time.
                let key = match body.get("key").and_then(Value::as_str).filter(|k| !k.is_empty()) {
                    Some(key) => {
                        let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, key)
                            .map_err(|_| invalid("A key must be base64."))?;
                        if decoded.is_empty() {
                            return Err(invalid("A key must not be empty."));
                        }
                        key.to_string()
                    }
                    None => store::new_key(),
                };
                let state = ctx.engine.state::<CosmosState>();
                let mut state = state.lock();
                let account = state
                    .accounts
                    .get_mut(&resource.name.to_ascii_lowercase())
                    .ok_or_else(|| ArmError::not_found("ResourceNotFound", "The account was not found."))?;
                account.keys[index] = key;
                Ok(Value::Null)
            }
            other => Err(ArmError::not_implemented(format_args!("the '{other}' action on {ACCOUNT_TYPE}"))),
        }
    }
}

fn with_account<T>(ctx: &ArmContext<'_>, id: &str, f: impl FnOnce(&mut Account) -> Result<T, ArmError>) -> Result<T, ArmError> {
    let name = id_part(id, "databaseAccounts").unwrap_or_default().to_ascii_lowercase();
    let state = ctx.engine.state::<CosmosState>();
    let mut state = state.lock();
    let account = state.accounts.get_mut(&name).ok_or_else(|| ArmError::not_found("ResourceNotFound", format!("The account '{name}' was not found.")))?;
    f(account)
}

fn options_throughput(body: &Value) -> Option<i64> {
    body.pointer("/properties/options/throughput")
        .and_then(Value::as_i64)
        .or_else(|| body.pointer("/properties/options/autoscaleSettings/maxThroughput").and_then(Value::as_i64))
}

/// The `options` a database or container reports: its own throughput, as the
/// service reports it on a read, when it has dedicated throughput (whether
/// set here or by replacing its offer on the data plane).
fn options_json(throughput: Option<i64>) -> Value {
    match throughput {
        Some(throughput) => json!({"throughput": throughput}),
        None => json!({}),
    }
}

pub(crate) struct SqlDatabases;

impl ResourceTypeHandler for SqlDatabases {
    fn resource_type(&self) -> &'static str {
        DATABASE_TYPE
    }

    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        let id = request.body.pointer("/properties/resource/id").and_then(Value::as_str).unwrap_or(request.name);
        if id != request.name {
            return Err(invalid("The resource id must match the database name."));
        }
        store::validate_id(id).map_err(invalid)?;
        let throughput = options_throughput(request.body);
        let resource = with_account(ctx, request.id, |a| {
            let database = a.databases.entry(id.to_string()).or_insert_with(|| Database::new(id));
            if throughput.is_some() {
                database.throughput = throughput;
            }
            let json = database.json();
            a.bump();
            Ok(json)
        })?;
        Ok(json!({"resource": resource, "options": {}}))
    }

    fn view(&self, ctx: &ArmContext<'_>, resource: &mut Resource) {
        let _ = with_account(ctx, &resource.id, |a| {
            if let Some(database) = a.databases.get(&resource.name) {
                resource.properties["resource"] = database.json();
                resource.properties["options"] = options_json(database.throughput);
            }
            Ok(())
        });
    }

    fn delete_status(&self) -> http::StatusCode {
        // The specification allows only 202 and 204.
        http::StatusCode::NO_CONTENT
    }

    fn deleted(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        let _ = with_account(ctx, &resource.id, |a| {
            a.databases.remove(&resource.name);
            a.bump();
            Ok(())
        });
    }
}

pub(crate) struct SqlContainers;

impl ResourceTypeHandler for SqlContainers {
    fn resource_type(&self) -> &'static str {
        CONTAINER_TYPE
    }

    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        let database = id_part(request.id, "sqlDatabases").unwrap_or_default().to_string();
        let mut definition: Map<String, Value> = request
            .body
            .pointer("/properties/resource")
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| invalid("The container needs properties.resource."))?;
        let id = definition.get("id").and_then(Value::as_str).unwrap_or(request.name).to_string();
        if id != request.name {
            return Err(invalid("The resource id must match the container name."));
        }
        definition.insert("id".into(), json!(id));
        let throughput = options_throughput(request.body);
        let resource = with_account(ctx, request.id, |a| {
            let db = a.databases.get_mut(&database).ok_or_else(|| ArmError::not_found("NotFound", format!("The database '{database}' was not found.")))?;
            let db_rid = db.system.rid.clone();
            let json = match db.containers.get_mut(&id) {
                Some(existing) => {
                    if definition.contains_key("partitionKey") && store::partition_key_paths(&definition) != existing.paths() {
                        return Err(invalid("The partition key of a container cannot change."));
                    }
                    let partition_key = existing.definition.get("partitionKey").cloned();
                    existing.definition = store::user_body(definition);
                    if let Some(pk) = partition_key {
                        existing.definition.insert("partitionKey".into(), pk);
                    }
                    if throughput.is_some() {
                        existing.throughput = throughput;
                    }
                    existing.system.touch();
                    existing.json(&db_rid)
                }
                None => {
                    let mut container = Container::new(store::user_body(definition)).map_err(invalid)?;
                    container.throughput = throughput;
                    let json = container.json(&db_rid);
                    db.containers.insert(id.clone(), container);
                    json
                }
            };
            a.bump();
            Ok(json)
        })?;
        Ok(json!({"resource": resource, "options": {}}))
    }

    fn view(&self, ctx: &ArmContext<'_>, resource: &mut Resource) {
        let database = id_part(&resource.id, "sqlDatabases").unwrap_or_default().to_string();
        let _ = with_account(ctx, &resource.id, |a| {
            if let Some(db) = a.databases.get(&database) {
                if let Some(container) = db.containers.get(&resource.name) {
                    resource.properties["resource"] = container.json(&db.system.rid);
                    resource.properties["options"] = options_json(container.throughput);
                }
            }
            Ok(())
        });
    }

    fn delete_status(&self) -> http::StatusCode {
        // The specification allows only 202 and 204.
        http::StatusCode::NO_CONTENT
    }

    fn deleted(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        let database = id_part(&resource.id, "sqlDatabases").unwrap_or_default().to_string();
        let _ = with_account(ctx, &resource.id, |a| {
            if let Some(db) = a.databases.get_mut(&database) {
                db.containers.remove(&resource.name);
            }
            a.bump();
            Ok(())
        });
    }
}
