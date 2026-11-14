//! Resource types served from their specification's schema alone: Storage
//! context caches and their containers, connectors, data shares and advanced
//! platform metrics. The schema is their behaviour, so these tests check the
//! rules it states: names, required and read-only members, types, bounds,
//! patterns, defaults, members fixed at creation, and PATCH merging.

use http::{Method, StatusCode};
use serde_json::{json, Value};

use crate::support::{Client, Reply, SUB};

const API: &str = "api-version=2026-06-01";

fn group_path(group: &str) -> String {
    format!("{SUB}/resourceGroups/{group}/providers/Microsoft.Storage")
}

fn refused(reply: &Reply, code: &str, fragment: &str) {
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text());
    assert_eq!(reply.error_code(), code, "{}", reply.text());
    assert!(reply.text().contains(fragment), "expected '{fragment}' in {}", reply.text());
}

fn ok(reply: &Reply) -> Value {
    assert!(reply.status.is_success(), "{}", reply.text());
    reply.json()
}

#[tokio::test]
async fn context_caches_follow_their_schema() {
    let client = Client::new();
    client.create_group("caches").await;
    let cache = |name: &str| format!("{}/contextCaches/{name}?{API}", group_path("caches"));
    let put = |name: &str, body: Value| {
        let path = cache(name);
        let client = &client;
        async move { client.arm(Method::PUT, &path, Some(body)).await }
    };

    refused(&put("Bad_Name", json!({"location": "eastus", "properties": {"accountKind": "x"}})).await, "InvalidResourceName", "pattern");
    refused(&put("cache-1", json!({"location": "eastus"})).await, "InvalidRequestContent", "'properties' is missing");
    refused(&put("cache-1", json!({"location": "eastus", "properties": {}})).await, "InvalidRequestContent", "properties.accountKind");
    refused(
        &put("cache-1", json!({"location": "eastus", "properties": {"accountKind": "x", "description": "d".repeat(251)}})).await,
        "InvalidRequestContent",
        "properties.description",
    );
    refused(&put("cache-1", json!({"location": "eastus", "properties": {"accountKind": 5}})).await, "InvalidRequestContent", "expected a string");

    let created = put(
        "cache-1",
        json!({"location": "eastus", "properties": {"accountKind": "StorageV2", "description": "first", "provisioningState": "Failed", "unknown": 1}}),
    )
    .await;
    let properties = &ok(&created)["properties"];
    assert_eq!(properties["provisioningState"], "Succeeded", "a read-only member is the service's to set");
    assert_eq!(properties["accountKind"], "StorageV2");
    assert!(properties.get("unknown").is_none(), "a member the schema does not know is dropped");

    // PATCH merges, and cannot change what is fixed at creation.
    let patched = ok(&client.arm(Method::PATCH, &cache("cache-1"), Some(json!({"properties": {"description": "second"}}))).await);
    assert_eq!((&patched["properties"]["description"], &patched["properties"]["accountKind"]), (&json!("second"), &json!("StorageV2")));
    refused(
        &client.arm(Method::PATCH, &cache("cache-1"), Some(json!({"properties": {"accountKind": "BlobStorage"}}))).await,
        "InvalidRequestContent",
        "cannot be changed",
    );
    refused(
        &put("cache-1", json!({"location": "eastus", "properties": {"accountKind": "BlobStorage"}})).await,
        "InvalidRequestContent",
        "cannot be changed",
    );
    // The same value is no change.
    let replaced = ok(&put("cache-1", json!({"location": "eastus", "properties": {"accountKind": "StorageV2"}})).await);
    assert!(replaced["properties"].get("description").is_none(), "a PUT replaces the properties");

    let in_group = ok(&client.arm(Method::GET, &format!("{}/contextCaches?{API}", group_path("caches")), None).await);
    assert_eq!(in_group["value"][0]["name"], "cache-1");
    let in_subscription = ok(&client.arm(Method::GET, &format!("{SUB}/providers/Microsoft.Storage/contextCaches?{API}"), None).await);
    assert!(in_subscription["value"].as_array().unwrap().iter().any(|c| c["name"] == "cache-1"));

    // Containers, under the cache.
    let container = |name: &str| format!("{}/contextCaches/cache-1/contextCacheContainers/{name}?{API}", group_path("caches"));
    for (ttl, fragment) in [(json!(0), "out of range"), (json!(31), "out of range"), (json!("5"), "an integer"), (json!(2.5), "an integer")] {
        let body = json!({"properties": {"modelName": "m", "provider": "p", "timeToLive": ttl}});
        refused(&client.arm(Method::PUT, &container("c-1"), Some(body)).await, "InvalidRequestContent", fragment);
    }
    let made = ok(&client.arm(Method::PUT, &container("c-1"), Some(json!({"properties": {"modelName": "m", "provider": "p", "timeToLive": 30}}))).await);
    assert_eq!(made["properties"]["timeToLive"], 30);
    refused(
        &client.arm(Method::PATCH, &container("c-1"), Some(json!({"properties": {"modelName": "other"}}))).await,
        "InvalidRequestContent",
        "cannot be changed",
    );
    let patched = ok(&client.arm(Method::PATCH, &container("c-1"), Some(json!({"properties": {"timeToLive": 1}}))).await);
    assert_eq!((&patched["properties"]["timeToLive"], &patched["properties"]["modelName"]), (&json!(1), &json!("m")));

    // Deleting the cache deletes its containers.
    assert!(client.arm(Method::DELETE, &cache("cache-1"), None).await.status.is_success());
    assert_eq!(client.arm(Method::GET, &container("c-1"), None).await.status, StatusCode::NOT_FOUND);
}

async fn account(client: &Client, group: &str, name: &str) -> String {
    client.create_group(group).await;
    let path = format!("{}/storageAccounts/{name}", group_path(group));
    let reply = client
        .arm(Method::PUT, &format!("{path}?api-version=2023-05-01"), Some(json!({"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}})))
        .await;
    assert!(reply.status.is_success(), "{}", reply.text());
    path
}

#[tokio::test]
async fn connectors_fill_defaults_and_keep_creation_values() {
    let client = Client::new();
    let account = account(&client, "connectors", "connectorsacct").await;
    let connector = format!("{account}/connectors/link-1?{API}");

    refused(
        &client.arm(Method::PUT, &connector, Some(json!({"location": "eastus", "properties": {"dataSourceType": "x", "source": {}}}))).await,
        "InvalidRequestContent",
        "properties.source.type",
    );
    let body = json!({"location": "eastus", "properties": {"dataSourceType": "Blob", "source": {"type": "s3", "bucket": "b"}}});
    let created = ok(&client.arm(Method::PUT, &connector, Some(body.clone())).await);
    let properties = &created["properties"];
    assert_eq!((&properties["state"], &properties["testConnection"]), (&json!("Active"), &json!(false)), "defaults are filled at creation");
    assert_eq!(properties["source"]["bucket"], "b", "an open object keeps members the schema cannot list");
    let created_at = properties["creationTime"].as_str().expect("a creation time").to_string();

    let mut update = body.clone();
    update["properties"]["state"] = json!("Disabled");
    let updated = ok(&client.arm(Method::PUT, &connector, Some(update)).await);
    assert_eq!(updated["properties"]["state"], "Disabled");
    assert_eq!(updated["properties"]["creationTime"], created_at, "the creation time is kept");
    refused(
        &client.arm(Method::PATCH, &connector, Some(json!({"properties": {"testConnection": "yes"}}))).await,
        "InvalidRequestContent",
        "true or false",
    );
}

#[tokio::test]
async fn data_shares_check_items_and_patterns() {
    let client = Client::new();
    let account = account(&client, "shares", "datasharesacct").await;
    let share = format!("{account}/dataShares/share-1?{API}");
    let policy = |principal: &str| json!({"principalId": principal, "tenantId": "00000000-0000-0000-0000-000000000001", "permission": "Read"});
    let body = |policies: Value, assets: Value| json!({"location": "eastus", "properties": {"accessPolicies": policies, "assets": assets}});
    let asset = json!([{"assetPath": "c/b", "displayName": "B"}]);

    refused(&client.arm(Method::PUT, &share, Some(body(json!([policy("not-a-guid")]), asset.clone()))).await, "InvalidRequestContent", "accessPolicies[0].principalId");
    refused(&client.arm(Method::PUT, &share, Some(body(json!([]), json!([{"assetPath": "x"}])))).await, "InvalidRequestContent", "assets[0].displayName");
    refused(&client.arm(Method::PUT, &share, Some(body(json!({}), asset.clone()))).await, "InvalidRequestContent", "an array");
    refused(
        &client.arm(Method::PUT, &share, Some(json!({"location": "eastus", "properties": {"assets": asset.clone()}}))).await,
        "InvalidRequestContent",
        "properties.accessPolicies",
    );

    let mut good = body(json!([policy("00000000-0000-0000-0000-0000000000aa")]), asset);
    good["properties"]["dataShareIdentifier"] = json!("00000000-0000-0000-0000-000000000000");
    let created = ok(&client.arm(Method::PUT, &share, Some(good)).await);
    assert_eq!(created["properties"]["accessPolicies"][0]["permission"], "Read");
    assert!(created["properties"].get("dataShareIdentifier").is_none(), "a read-only member the caller sends is not kept");
    let listed = ok(&client.arm(Method::GET, &format!("{account}/dataShares?{API}"), None).await);
    assert_eq!(listed["value"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn advanced_platform_metrics_check_types() {
    let client = Client::new();
    let account = account(&client, "metrics", "metricsacct1").await;
    let rule = format!("{account}/advancedPlatformMetrics/default?{API}");
    refused(
        &client.arm(Method::PUT, &rule, Some(json!({"properties": {"enabled": "yes", "ruleConfig": {}}}))).await,
        "InvalidRequestContent",
        "true or false",
    );
    refused(
        &client.arm(Method::PUT, &rule, Some(json!({"properties": {"enabled": true, "ruleConfig": {"filterValues": "a"}}}))).await,
        "InvalidRequestContent",
        "an array",
    );
    refused(&client.arm(Method::PUT, &rule, Some(json!({"properties": {"enabled": true}}))).await, "InvalidRequestContent", "ruleConfig");
    let created = ok(
        &client
            .arm(Method::PUT, &rule, Some(json!({"properties": {"enabled": true, "ruleConfig": {"filterType": "Prefix", "filterValues": ["logs/"]}}})))
            .await,
    );
    assert_eq!(created["properties"]["ruleConfig"]["filterValues"], json!(["logs/"]));
    assert!(created["properties"]["lastModifiedTime"].is_string(), "a read-only modification time is stamped");
}
