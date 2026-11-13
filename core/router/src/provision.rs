//! Provisioning from a configuration file at startup (`--config`), so a
//! compose file or CI job gets its resources without a setup script.
//!
//! Each entry becomes an ordinary Resource Manager request sent to the
//! emulator in-process, so it is validated exactly as a client's would be.
//!
//! ```json
//! {
//!   "subscription": "00000000-0000-0000-0000-000000000000",
//!   "serviceBus": [{
//!     "resourceGroup": "local",
//!     "namespace": "local",
//!     "location": "eastus",
//!     "sku": "Standard",
//!     "keys": { "RootManageSharedAccessKey": "my-key" },
//!     "queues": [{ "name": "jobs", "properties": { "maxDeliveryCount": 5 } }],
//!     "topics": [{
//!       "name": "events",
//!       "subscriptions": [{
//!         "name": "audit",
//!         "rules": [{ "name": "errors", "properties": { "filterType": "SqlFilter", "sqlFilter": { "sqlExpression": "level = 'error'" } } }]
//!       }]
//!     }]
//!   }],
//!   "eventHubs": [{
//!     "resourceGroup": "local",
//!     "namespace": "local-hubs",
//!     "keys": { "RootManageSharedAccessKey": "my-key" },
//!     "hubs": [{ "name": "telemetry", "partitionCount": 4, "consumerGroups": ["analytics"] }]
//!   }],
//!   "cosmos": [{
//!     "resourceGroup": "local",
//!     "account": "local-cosmos",
//!     "key": "<base64 key>",
//!     "databases": [{ "id": "app", "containers": [{ "id": "orders", "partitionKey": "/tenant" }] }]
//!   }],
//!   "resources": [
//!     { "id": "/subscriptions/.../resourceGroups/local/providers/Microsoft.Storage/storageAccounts/files",
//!       "apiVersion": "2023-05-01", "body": { "location": "eastus", "kind": "StorageV2", "sku": { "name": "Standard_LRS" } } }
//!   ]
//! }
//! ```
//!
//! A key under `keys` is either one string (the primary key) or
//! `{ "primary": "...", "secondary": "..." }`.

use axum::body::Body;
use http::{Method, Request};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::Emulator;

const SERVICE_BUS_API: &str = "2022-10-01-preview";
const COSMOS_API: &str = "2024-11-15";
const EVENT_HUBS_API: &str = "2024-01-01";
const RESOURCES_API: &str = "2021-04-01";

/// One Resource Manager request the file asks for.
struct Step {
    method: Method,
    path: String,
    body: Value,
}

fn put(path: String, api: &str, body: Value) -> Step {
    Step { method: Method::PUT, path: format!("{path}?api-version={api}"), body }
}

fn text<'a>(value: &'a Value, field: &str, context: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{context}: '{field}' is required"))
}

fn properties(value: &Value) -> Value {
    json!({ "properties": value.get("properties").cloned().unwrap_or_else(|| json!({})) })
}

fn list<'a>(value: &'a Value, field: &str) -> impl Iterator<Item = &'a Value> {
    value.get(field).and_then(Value::as_array).into_iter().flatten()
}

fn steps(file: &Value, default_subscription: &str) -> Result<Vec<Step>, String> {
    let subscription = file.get("subscription").and_then(Value::as_str).unwrap_or(default_subscription);
    let mut steps = Vec::new();
    let mut groups = Vec::new();
    for bus in list(file, "serviceBus") {
        let group = text(bus, "resourceGroup", "serviceBus entry")?;
        let name = text(bus, "namespace", "serviceBus entry")?;
        let location = bus.get("location").and_then(Value::as_str).unwrap_or("eastus");
        let group_path = format!("/subscriptions/{subscription}/resourceGroups/{group}");
        if !groups.contains(&group_path) {
            groups.push(group_path.clone());
            steps.push(put(group_path.clone(), RESOURCES_API, json!({ "location": location })));
        }
        let namespace = format!("{group_path}/providers/Microsoft.ServiceBus/namespaces/{name}");
        let sku = bus.get("sku").and_then(Value::as_str).unwrap_or("Standard");
        steps.push(put(namespace.clone(), SERVICE_BUS_API, json!({ "location": location, "sku": { "name": sku, "tier": sku } })));
        for queue in list(bus, "queues") {
            let queue_name = text(queue, "name", "queue")?;
            steps.push(put(format!("{namespace}/queues/{queue_name}"), SERVICE_BUS_API, properties(queue)));
        }
        for topic in list(bus, "topics") {
            let topic_name = text(topic, "name", "topic")?;
            let topic_path = format!("{namespace}/topics/{topic_name}");
            steps.push(put(topic_path.clone(), SERVICE_BUS_API, properties(topic)));
            for subscription in list(topic, "subscriptions") {
                let sub_name = text(subscription, "name", "subscription")?;
                let sub_path = format!("{topic_path}/subscriptions/{sub_name}");
                steps.push(put(sub_path.clone(), SERVICE_BUS_API, properties(subscription)));
                let rules: Vec<&Value> = list(subscription, "rules").collect();
                if !rules.is_empty() {
                    // Rules given replace the catch-all `$Default`.
                    steps.push(Step {
                        method: Method::DELETE,
                        path: format!("{sub_path}/rules/$Default?api-version={SERVICE_BUS_API}"),
                        body: Value::Null,
                    });
                }
                for rule in rules {
                    let rule_name = text(rule, "name", "rule")?;
                    steps.push(put(format!("{sub_path}/rules/{rule_name}"), SERVICE_BUS_API, properties(rule)));
                }
            }
        }
        for (rule, keys) in bus.get("keys").and_then(Value::as_object).into_iter().flatten() {
            let rule_path = format!("{namespace}/AuthorizationRules/{rule}");
            if rule != "RootManageSharedAccessKey" {
                steps.push(put(rule_path.clone(), SERVICE_BUS_API, json!({ "properties": { "rights": ["Listen", "Send", "Manage"] } })));
            }
            let pairs = match keys {
                Value::String(primary) => vec![("PrimaryKey", primary.as_str())],
                Value::Object(keys) => [("PrimaryKey", "primary"), ("SecondaryKey", "secondary")]
                    .into_iter()
                    .filter_map(|(kind, field)| keys.get(field).and_then(Value::as_str).map(|k| (kind, k)))
                    .collect(),
                _ => return Err(format!("keys.{rule}: a key string or {{\"primary\", \"secondary\"}}")),
            };
            for (kind, key) in pairs {
                steps.push(Step {
                    method: Method::POST,
                    path: format!("{rule_path}/regenerateKeys?api-version={SERVICE_BUS_API}"),
                    body: json!({ "keyType": kind, "key": key }),
                });
            }
        }
    }
    for hubs in list(file, "eventHubs") {
        let group = text(hubs, "resourceGroup", "eventHubs entry")?;
        let name = text(hubs, "namespace", "eventHubs entry")?;
        let location = hubs.get("location").and_then(Value::as_str).unwrap_or("eastus");
        let group_path = format!("/subscriptions/{subscription}/resourceGroups/{group}");
        if !groups.contains(&group_path) {
            groups.push(group_path.clone());
            steps.push(put(group_path.clone(), RESOURCES_API, json!({ "location": location })));
        }
        let namespace = format!("{group_path}/providers/Microsoft.EventHub/namespaces/{name}");
        let sku = hubs.get("sku").and_then(Value::as_str).unwrap_or("Standard");
        steps.push(put(namespace.clone(), EVENT_HUBS_API, json!({ "location": location, "sku": { "name": sku, "tier": sku } })));
        for hub in list(hubs, "hubs") {
            let hub_name = text(hub, "name", "event hub")?;
            let hub_path = format!("{namespace}/eventhubs/{hub_name}");
            let mut properties = hub.get("properties").cloned().unwrap_or_else(|| json!({}));
            for member in ["partitionCount", "messageRetentionInDays"] {
                if let Some(value) = hub.get(member) {
                    properties[member] = value.clone();
                }
            }
            steps.push(put(hub_path.clone(), EVENT_HUBS_API, json!({ "properties": properties })));
            for consumer_group in hub.get("consumerGroups").and_then(Value::as_array).into_iter().flatten() {
                let consumer_group = consumer_group.as_str().ok_or_else(|| format!("{hub_name}: consumer groups are names"))?;
                if consumer_group != "$Default" {
                    steps.push(put(format!("{hub_path}/consumergroups/{consumer_group}"), EVENT_HUBS_API, json!({})));
                }
            }
        }
        for (rule, keys) in hubs.get("keys").and_then(Value::as_object).into_iter().flatten() {
            let rule_path = format!("{namespace}/authorizationRules/{rule}");
            if rule != "RootManageSharedAccessKey" {
                steps.push(put(rule_path.clone(), EVENT_HUBS_API, json!({ "properties": { "rights": ["Listen", "Send", "Manage"] } })));
            }
            let pairs = match keys {
                Value::String(primary) => vec![("PrimaryKey", primary.as_str())],
                Value::Object(keys) => [("PrimaryKey", "primary"), ("SecondaryKey", "secondary")]
                    .into_iter()
                    .filter_map(|(kind, field)| keys.get(field).and_then(Value::as_str).map(|k| (kind, k)))
                    .collect(),
                _ => return Err(format!("keys.{rule}: a key string or {{\"primary\", \"secondary\"}}")),
            };
            for (kind, key) in pairs {
                steps.push(Step {
                    method: Method::POST,
                    path: format!("{rule_path}/regenerateKeys?api-version={EVENT_HUBS_API}"),
                    body: json!({ "keyType": kind, "key": key }),
                });
            }
        }
    }
    for cosmos in list(file, "cosmos") {
        let group = text(cosmos, "resourceGroup", "cosmos entry")?;
        let name = text(cosmos, "account", "cosmos entry")?;
        let location = cosmos.get("location").and_then(Value::as_str).unwrap_or("eastus");
        let group_path = format!("/subscriptions/{subscription}/resourceGroups/{group}");
        if !groups.contains(&group_path) {
            groups.push(group_path.clone());
            steps.push(put(group_path.clone(), RESOURCES_API, json!({ "location": location })));
        }
        let account = format!("{group_path}/providers/Microsoft.DocumentDB/databaseAccounts/{name}");
        steps.push(put(
            account.clone(),
            COSMOS_API,
            json!({ "location": location, "properties": { "databaseAccountOfferType": "Standard", "locations": [{ "locationName": location }] } }),
        ));
        for (field, kind) in [("key", "primary"), ("secondaryKey", "secondary"), ("readonlyKey", "primaryReadonly")] {
            if let Some(key) = cosmos.get(field).and_then(Value::as_str) {
                steps.push(Step {
                    method: Method::POST,
                    path: format!("{account}/regenerateKey?api-version={COSMOS_API}"),
                    body: json!({ "keyKind": kind, "key": key }),
                });
            }
        }
        for database in list(cosmos, "databases") {
            let id = text(database, "id", "cosmos database")?;
            let database_path = format!("{account}/sqlDatabases/{id}");
            let mut body = json!({ "properties": { "resource": { "id": id }, "options": {} } });
            if let Some(throughput) = database.get("throughput") {
                body["properties"]["options"]["throughput"] = throughput.clone();
            }
            steps.push(put(database_path.clone(), COSMOS_API, body));
            for container in list(database, "containers") {
                let container_id = text(container, "id", "cosmos container")?;
                let mut resource = container.as_object().cloned().unwrap_or_default();
                resource.remove("throughput");
                // "partitionKey": "/tenant" is short for {"paths": ["/tenant"]}.
                if let Some(Value::String(path)) = resource.get("partitionKey").cloned() {
                    resource.insert("partitionKey".into(), json!({ "paths": [path], "kind": "Hash" }));
                }
                let mut body = json!({ "properties": { "resource": resource, "options": {} } });
                if let Some(throughput) = container.get("throughput") {
                    body["properties"]["options"]["throughput"] = throughput.clone();
                }
                steps.push(put(format!("{database_path}/containers/{container_id}"), COSMOS_API, body));
            }
        }
    }
    for resource in list(file, "resources") {
        let id = text(resource, "id", "resource")?;
        let api = text(resource, "apiVersion", id)?;
        steps.push(put(id.to_string(), api, resource.get("body").cloned().unwrap_or_else(|| json!({}))));
    }
    Ok(steps)
}

/// Provisions what `file` describes. Returns one line per request made.
pub async fn provision(emulator: &Emulator, file: &Value) -> Result<Vec<String>, String> {
    let config = emulator.config();
    let token = emulator_core::auth::issue_token(config, "https://management.azure.com/", emulator_core::auth::PROVISIONING_CLIENT);
    let host = format!("{}:{}", config.host, config.port);
    let mut done = Vec::new();
    for step in steps(file, &config.subscription_id)? {
        let body = if step.body.is_null() { Body::empty() } else { Body::from(step.body.to_string()) };
        let request = Request::builder()
            .method(step.method.clone())
            .uri(&step.path)
            .header("host", &host)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(body)
            .map_err(|e| format!("{}: {e}", step.path))?;
        let response = emulator.router().oneshot(request).await.map_err(|e| format!("{}: {e}", step.path))?;
        let status = response.status();
        // Deleting a `$Default` rule that is already gone is fine.
        if !status.is_success() && !(step.method == Method::DELETE && status == http::StatusCode::NOT_FOUND) {
            let body = axum::body::to_bytes(response.into_body(), 64 * 1024).await.unwrap_or_default();
            return Err(format!("{} {}: {status} {}", step.method, step.path, String::from_utf8_lossy(&body)));
        }
        done.push(format!("{} {}", step.method, step.path.split('?').next().unwrap_or_default()));
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_bus_shorthand_expands_to_requests() {
        let file = json!({
            "serviceBus": [{
                "resourceGroup": "local", "namespace": "bus",
                "keys": { "RootManageSharedAccessKey": "k1", "app": { "primary": "k2", "secondary": "k3" } },
                "queues": [{ "name": "jobs" }],
                "topics": [{ "name": "events", "subscriptions": [{ "name": "audit", "rules": [{ "name": "r", "properties": {} }] }] }]
            }]
        });
        let lines: Vec<String> = steps(&file, "sub")
            .unwrap()
            .iter()
            .map(|s| format!("{} {}", s.method, s.path.split('?').next().unwrap().trim_start_matches("/subscriptions/sub/resourceGroups/local")))
            .collect();
        let ns = "/providers/Microsoft.ServiceBus/namespaces/bus";
        assert_eq!(
            lines,
            vec![
                "PUT ".to_string(),
                format!("PUT {ns}"),
                format!("PUT {ns}/queues/jobs"),
                format!("PUT {ns}/topics/events"),
                format!("PUT {ns}/topics/events/subscriptions/audit"),
                format!("DELETE {ns}/topics/events/subscriptions/audit/rules/$Default"),
                format!("PUT {ns}/topics/events/subscriptions/audit/rules/r"),
                format!("POST {ns}/AuthorizationRules/RootManageSharedAccessKey/regenerateKeys"),
                format!("PUT {ns}/AuthorizationRules/app"),
                format!("POST {ns}/AuthorizationRules/app/regenerateKeys"),
                format!("POST {ns}/AuthorizationRules/app/regenerateKeys"),
            ]
        );
    }

    #[test]
    fn missing_names_are_reported() {
        let file = json!({ "serviceBus": [{ "resourceGroup": "local" }] });
        assert!(steps(&file, "sub").err().unwrap().contains("'namespace' is required"));
    }
}
