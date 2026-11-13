//! Replays the specification's own worked examples against the emulator.
//!
//! `emulator-scaffold` extracts every create (`PUT`) example a provider's
//! specification gives into `services/<name>/spec-examples.json`. For each type
//! the emulator serves (by hand-written behaviour or generically from the
//! schema), this test creates the example's parent resources from their own
//! examples, sends the example request, and checks that the status is one the
//! operation answers with and that every setting sent comes back unchanged.
//!
//! Examples expected to fail are listed, with the reason, in
//! `tests/spec-examples-known-failures.txt`. The test fails on any other
//! failure, and on a listed example that now passes, so the list stays true.

use std::collections::{BTreeMap, BTreeSet};

use http::{Method, StatusCode};
use serde_json::{json, Value};

use crate::support::Client;

const EXAMPLES: [&str; 4] = [
    include_str!("../../../../services/storage/spec-examples.json"),
    include_str!("../../../../services/keyvault/spec-examples.json"),
    include_str!("../../../../services/servicebus/spec-examples.json"),
    include_str!("../../../../services/managedidentity/spec-examples.json"),
];
const KNOWN_FAILURES: &str = include_str!("../../spec-examples-known-failures.txt");

struct Example {
    kind: String,
    operation: String,
    title: String,
    path: String,
    parameters: BTreeMap<String, String>,
    body: Value,
    statuses: Vec<u16>,
}

fn load() -> Vec<Example> {
    EXAMPLES
        .iter()
        .flat_map(|text| serde_json::from_str::<Vec<Value>>(text).expect("valid spec-examples.json"))
        .map(|e| Example {
            kind: e["type"].as_str().unwrap_or("").to_string(),
            operation: e["operationId"].as_str().unwrap_or("").to_string(),
            title: e["title"].as_str().unwrap_or("").to_string(),
            path: e["path"].as_str().unwrap_or("").to_string(),
            parameters: e["parameters"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect(),
            body: e["body"].clone(),
            statuses: e["statuses"].as_array().into_iter().flatten().filter_map(|s| s.as_u64()).map(|s| s as u16).collect(),
        })
        .collect()
}

/// The example's path with its parameters (and this emulator's subscription).
fn fill(template: &str, parameters: &BTreeMap<String, String>, fallback: &BTreeMap<String, String>, subscription: &str) -> String {
    let mut out = String::new();
    for segment in template.split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        match segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            Some("subscriptionId") => out.push_str(subscription),
            Some(name) => out.push_str(parameters.get(name).or_else(|| fallback.get(name)).map(String::as_str).unwrap_or(name)),
            None => out.push_str(segment),
        }
    }
    out
}

fn parent_type(kind: &str) -> Option<String> {
    let (namespace, types) = kind.split_once('/')?;
    let (parent, _) = types.rsplit_once('/')?;
    Some(format!("{namespace}/{parent}"))
}

/// `value` with every resource id moved into `subscription`: the emulator has
/// one subscription, and examples use made-up ones and placeholders.
fn in_this_subscription(value: &Value, subscription: &str) -> Value {
    let rewrite = |text: &str| -> String {
        let mut parts: Vec<&str> = text.split('/').collect();
        if text.starts_with("/subscriptions/") && parts.len() > 2 {
            parts[2] = subscription;
        }
        parts.join("/")
    };
    match value {
        Value::String(text) => Value::String(rewrite(text)),
        Value::Array(items) => Value::Array(items.iter().map(|v| in_this_subscription(v, subscription)).collect()),
        Value::Object(members) => Value::Object(
            members.iter().map(|(k, v)| (rewrite(k), in_this_subscription(v, subscription))).collect(),
        ),
        other => other.clone(),
    }
}

struct Run<'a> {
    client: &'a Client,
    examples: &'a [Example],
    served: &'a BTreeSet<String>,
}

impl Run<'_> {
    async fn send(&self, example: &Example, parameters: &BTreeMap<String, String>) -> crate::support::Reply {
        let path = fill(&example.path, parameters, &example.parameters, &self.client.config().subscription_id);
        let version = parameters.get("api-version").or_else(|| example.parameters.get("api-version")).cloned().unwrap_or_default();
        let body = (!example.body.is_null()).then(|| in_this_subscription(&example.body, &self.client.config().subscription_id));
        if let Some(body) = &body {
            self.create_identities(body).await;
        }
        self.client.arm(Method::PUT, &format!("{path}?api-version={version}"), body).await
    }

    /// Creates the user-assigned identities a request body refers to, as a
    /// caller would have before sending it.
    async fn create_identities(&self, body: &Value) {
        let ids: Vec<String> = body
            .pointer("/identity/userAssignedIdentities")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let group = id.split('/').nth(4).unwrap_or("rg");
            let path = format!("/subscriptions/{}/resourcegroups/{group}?api-version=2021-04-01", self.client.config().subscription_id);
            self.client.arm(Method::PUT, &path, Some(json!({"location": "eastus"}))).await;
            self.client.arm(Method::PUT, &format!("{id}?api-version=2024-11-30"), Some(json!({"location": "eastus"}))).await;
        }
    }

    /// Creates `kind` and its ancestors from their first examples, named by
    /// `parameters` where those name them. Returns why that failed, if it did.
    async fn ensure(&self, kind: &str, parameters: &BTreeMap<String, String>) -> Result<(), String> {
        if let Some(parent) = parent_type(kind) {
            Box::pin(self.ensure(&parent, parameters)).await?;
        } else if let Some(group) = parameters.get("resourceGroupName") {
            let path = format!("/subscriptions/{}/resourcegroups/{group}?api-version=2021-04-01", self.client.config().subscription_id);
            self.client.arm(Method::PUT, &path, Some(json!({"location": "eastus"}))).await;
        }
        let Some(example) = self.examples.iter().find(|e| e.kind == kind) else {
            return Ok(());
        };
        if !self.served.contains(&kind.to_ascii_lowercase()) {
            return Err(format!("parent {kind} is not served"));
        }
        let reply = self.send(example, parameters).await;
        match reply.status.is_success() {
            true => Ok(()),
            false => Err(format!("creating parent {kind} failed: {} {}", reply.status, reply.text())),
        }
    }

    async fn check(&self, example: &Example) -> Result<(), String> {
        if let Some(parent) = parent_type(&example.kind) {
            self.ensure(&parent, &example.parameters).await?;
        } else if let Some(group) = example.parameters.get("resourceGroupName") {
            let path = format!("/subscriptions/{}/resourcegroups/{group}?api-version=2021-04-01", self.client.config().subscription_id);
            self.client.arm(Method::PUT, &path, Some(json!({"location": "eastus"}))).await;
        }
        let reply = self.send(example, &example.parameters).await;
        let allowed = example.statuses.is_empty() || example.statuses.contains(&reply.status.as_u16());
        if !reply.status.is_success() || !allowed {
            return Err(format!("{} {}", reply.status, reply.text()));
        }
        if reply.status == StatusCode::ACCEPTED || reply.body.is_empty() {
            return Ok(());
        }
        // Every setting sent comes back. The service may add to it (a default
        // inside an object, permissions already granted), but not change it.
        let body = in_this_subscription(&example.body, &self.client.config().subscription_id);
        let sent = body.get("properties").and_then(Value::as_object);
        let returned = reply.json();
        for (name, value) in sent.into_iter().flatten() {
            if let Some(back) = returned.pointer(&format!("/properties/{name}")) {
                if !contains(back, value) {
                    return Err(format!("properties.{name} was sent as {value} and returned as {back}"));
                }
            }
        }
        Ok(())
    }
}

/// Whether `returned` carries everything in `sent`: objects member by member,
/// arrays element by element (each sent element within some returned one).
fn contains(returned: &Value, sent: &Value) -> bool {
    match (returned, sent) {
        (Value::Object(returned), Value::Object(sent)) => sent
            .iter()
            .all(|(key, value)| value.is_null() || returned.get(key).is_some_and(|back| contains(back, value))),
        (Value::Array(returned), Value::Array(sent)) => sent.iter().all(|value| returned.iter().any(|back| contains(back, value))),
        (returned, sent) => returned == sent,
    }
}

#[tokio::test]
async fn specification_examples() {
    let examples = load();
    let served: BTreeSet<String> = Client::new()
        .emulator
        .coverage()
        .iter()
        .filter(|c| (c.implemented || c.generic) && c.operation.method == "PUT")
        .filter_map(|c| c.resource_type)
        .map(str::to_ascii_lowercase)
        .collect();
    let known: BTreeMap<String, String> = KNOWN_FAILURES
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let (key, reason) = line.split_once(" -- ")?;
            Some((key.trim().to_string(), reason.trim().to_string()))
        })
        .collect();

    let (mut passed, mut problems) = (0, Vec::new());
    let mut listed_seen = BTreeSet::new();
    for example in examples.iter().filter(|e| served.contains(&e.kind.to_ascii_lowercase())) {
        let client = Client::new();
        let run = Run { client: &client, examples: &examples, served: &served };
        let key = format!("{} / {}", example.operation, example.title);
        let result = run.check(example).await;
        match (result, known.contains_key(&key)) {
            (Ok(()), false) => passed += 1,
            (Ok(()), true) => problems.push(format!("listed as failing but passes: {key}")),
            (Err(_), true) => {
                listed_seen.insert(key);
            }
            (Err(reason), false) => problems.push(format!("{key}: {reason}")),
        }
    }
    for key in known.keys().filter(|k| !listed_seen.contains(*k)) {
        if !problems.iter().any(|p| p.ends_with(key.as_str())) {
            problems.push(format!("listed but no longer an example of a served type: {key}"));
        }
    }
    println!("{passed} specification examples passed; {} known failures", listed_seen.len());
    assert!(problems.is_empty(), "{} problem(s):\n{}", problems.len(), problems.join("\n"));
}
