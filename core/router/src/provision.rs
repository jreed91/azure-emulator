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
