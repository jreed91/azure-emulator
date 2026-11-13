//! What a Cosmos DB account holds: databases, containers and items. The
//! Resource Manager resources (`sqlDatabases`, `containers`) and the data
//! plane read and write this same store.

use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use rand::RngCore;
use serde_json::{json, Map, Value};

/// The emulator state of one account, keyed by lowercased account name.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct CosmosState {
    pub(crate) accounts: HashMap<String, Account>,
}

/// An account's keys and data.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Account {
    /// Primary, secondary, primary read-only, secondary read-only.
    pub keys: [String; 4],
    pub databases: BTreeMap<String, Database>,
    /// The last logical sequence number: session tokens report it.
    pub lsn: u64,
}

impl Account {
    pub fn new() -> Self {
        Account { keys: [new_key(), new_key(), new_key(), new_key()], databases: BTreeMap::new(), lsn: 0 }
    }

    pub fn bump(&mut self) -> u64 {
        self.lsn += 1;
        self.lsn
    }
}

/// A new 512-bit account key, base64 encoded, as Cosmos DB issues them.
mod item_list {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::Item;

    pub(super) fn serialize<S: Serializer>(items: &BTreeMap<(String, String), Item>, serializer: S) -> Result<S::Ok, S::Error> {
        items.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<BTreeMap<(String, String), Item>, D::Error> {
        Ok(Vec::<((String, String), Item)>::deserialize(deserializer)?.into_iter().collect())
    }
}

pub(crate) fn new_key() -> String {
    let mut bytes = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub(crate) fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// A resource id in the style Cosmos DB uses: short, base64.
pub(crate) fn new_rid(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::STANDARD.encode(bytes).replace('/', "-")
}

pub(crate) fn new_etag() -> String {
    format!("\"{}\"", uuid::Uuid::new_v4())
}

/// The system properties every resource carries.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct System {
    pub rid: String,
    pub ts: i64,
    pub etag: String,
}

impl System {
    pub fn new() -> Self {
        System { rid: new_rid(6), ts: now(), etag: new_etag() }
    }

    pub fn touch(&mut self) {
        self.ts = now();
        self.etag = new_etag();
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Database {
    pub id: String,
    pub system: System,
    pub containers: BTreeMap<String, Container>,
    pub throughput: Option<i64>,
}

impl Database {
    pub fn new(id: &str) -> Self {
        Database { id: id.to_string(), system: System::new(), containers: BTreeMap::new(), throughput: None }
    }

    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "_rid": self.system.rid,
            "_self": format!("dbs/{}/", self.system.rid),
            "_etag": self.system.etag,
            "_colls": "colls/",
            "_users": "users/",
            "_ts": self.system.ts,
        })
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Container {
    /// The definition as given (id, partitionKey, indexingPolicy,
    /// defaultTtl, uniqueKeyPolicy, ...), without system properties.
    pub definition: Map<String, Value>,
    pub system: System,
    /// Items by (partition key, id), in insertion order of the key; a
    /// snapshot keeps them as a list, since JSON keys are strings.
    #[serde(with = "item_list")]
    pub items: BTreeMap<(String, String), Item>,
    pub throughput: Option<i64>,
    /// The etag of the partition key ranges: there is always one range.
    pub ranges_etag: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Item {
    pub body: Map<String, Value>,
    pub system: System,
    /// The logical sequence number of the last write, for the change feed.
    pub lsn: u64,
}

impl Container {
    pub fn new(mut definition: Map<String, Value>) -> Result<Self, String> {
        let id = definition.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or("The container needs an id.")?.to_string();
        validate_id(&id)?;
        let paths = partition_key_paths(&definition);
        if paths.is_empty() {
            // Containers without a partition key are treated as having the
            // legacy single partition.
            definition.insert("partitionKey".into(), json!({"paths": ["/_partitionKey"], "kind": "Hash", "systemKey": true}));
        } else {
            let pk = definition.get_mut("partitionKey").and_then(Value::as_object_mut).expect("checked above");
            pk.entry("kind").or_insert(json!(if paths.len() > 1 { "MultiHash" } else { "Hash" }));
            pk.entry("version").or_insert(json!(2));
        }
        definition.entry("indexingPolicy").or_insert(json!({
            "indexingMode": "consistent",
            "automatic": true,
            "includedPaths": [{"path": "/*"}],
            "excludedPaths": [{"path": "/\"_etag\"/?"}],
        }));
        definition.entry("conflictResolutionPolicy").or_insert(json!({
            "mode": "LastWriterWins",
            "conflictResolutionPath": "/_ts",
            "conflictResolutionProcedure": "",
        }));
        definition.entry("geospatialConfig").or_insert(json!({"type": "Geography"}));
        Ok(Container { definition, system: System::new(), items: BTreeMap::new(), throughput: None, ranges_etag: new_etag() })
    }

    pub fn id(&self) -> &str {
        self.definition.get("id").and_then(Value::as_str).unwrap_or_default()
    }

    pub fn json(&self, database_rid: &str) -> Value {
        let mut body = self.definition.clone();
        body.insert("_rid".into(), json!(self.system.rid));
        body.insert("_self".into(), json!(format!("dbs/{database_rid}/colls/{}/", self.system.rid)));
        body.insert("_etag".into(), json!(self.system.etag));
        body.insert("_ts".into(), json!(self.system.ts));
        for (key, value) in [("_docs", "docs/"), ("_sprocs", "sprocs/"), ("_triggers", "triggers/"), ("_udfs", "udfs/"), ("_conflicts", "conflicts/")] {
            body.insert(key.into(), json!(value));
        }
        Value::Object(body)
    }

    pub fn paths(&self) -> Vec<String> {
        partition_key_paths(&self.definition)
    }

    /// The partition key value `item` has under this container's paths, as
    /// the JSON array the SDKs send in `x-ms-documentdb-partitionkey`: an
    /// undefined value is `{}`.
    pub fn key_of(&self, item: &Map<String, Value>) -> Value {
        let values: Vec<Value> = self
            .paths()
            .iter()
            .map(|path| lookup_path(item, path).cloned().unwrap_or_else(|| json!({})))
            .collect();
        Value::Array(values)
    }

    /// The default time to live in seconds: `None` when items never expire.
    pub fn default_ttl(&self) -> Option<i64> {
        self.definition.get("defaultTtl").and_then(Value::as_i64)
    }

    /// Removes items whose time to live has passed.
    pub fn expire(&mut self) {
        let Some(default) = self.default_ttl() else { return };
        let now = now();
        self.items.retain(|_, item| {
            let ttl = item.body.get("ttl").and_then(Value::as_i64).unwrap_or(default);
            ttl < 0 || item.system.ts + ttl > now
        });
    }

    /// The unique key paths each item must not share within its partition.
    pub fn unique_keys(&self) -> Vec<Vec<String>> {
        self.definition
            .get("uniqueKeyPolicy")
            .and_then(|p| p.get("uniqueKeys"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|key| key.get("paths").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect())
            .collect()
    }
}

/// The partition key paths a container definition declares.
pub(crate) fn partition_key_paths(definition: &Map<String, Value>) -> Vec<String> {
    definition
        .get("partitionKey")
        .and_then(|pk| pk.get("paths"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// The value at a `/a/b` path of an item.
pub(crate) fn lookup_path<'a>(item: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    let mut segments = path.trim_start_matches('/').split('/');
    let first = segments.next()?;
    let mut current = item.get(unescape(first).as_str())?;
    for segment in segments {
        current = match current {
            Value::Object(map) => map.get(unescape(segment).as_str())?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

fn unescape(segment: &str) -> String {
    segment.trim_matches('"').replace("~1", "/").replace("~0", "~")
}

/// The store's key for a partition key value: its compact JSON.
pub(crate) fn key_string(key: &Value) -> String {
    key.to_string()
}

/// Ids may not contain `/`, `\`, `?` or `#`, nor end with a space.
pub(crate) fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 255 {
        return Err("The id must be between 1 and 255 characters.".into());
    }
    if id.contains(['/', '\\', '?', '#']) || id.ends_with(' ') {
        return Err(format!("The id '{id}' contains an invalid character: '/', '\\', '?' and '#' are not allowed, nor a trailing space."));
    }
    Ok(())
}

impl Item {
    pub fn json(&self, database_rid: &str, container_rid: &str) -> Value {
        let mut body = self.body.clone();
        body.insert("_rid".into(), json!(self.system.rid));
        body.insert("_self".into(), json!(format!("dbs/{database_rid}/colls/{container_rid}/docs/{}/", self.system.rid)));
        body.insert("_etag".into(), json!(self.system.etag));
        body.insert("_attachments".into(), json!("attachments/"));
        body.insert("_ts".into(), json!(self.system.ts));
        Value::Object(body)
    }
}

/// Strips the system properties a client sends back with an item.
pub(crate) fn user_body(mut body: Map<String, Value>) -> Map<String, Value> {
    for key in ["_rid", "_self", "_etag", "_attachments", "_ts", "_lsn"] {
        body.remove(key);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_keys_and_paths() {
        let container = Container::new(json!({"id": "c", "partitionKey": {"paths": ["/tenant/id"]}}).as_object().unwrap().clone()).unwrap();
        let item = json!({"id": "1", "tenant": {"id": "t1"}});
        assert_eq!(container.key_of(item.as_object().unwrap()), json!(["t1"]));
        assert_eq!(container.key_of(json!({"id": "2"}).as_object().unwrap()), json!([{}]));
        assert_eq!(container.definition["partitionKey"]["kind"], "Hash");
        assert!(validate_id("a/b").is_err());
        assert!(validate_id("ok").is_ok());
    }
}
