use http::{Method, StatusCode};
use serde_json::{json, Value};

use crate::support::{Client, Reply, SUB};

const API: &str = "api-version=2024-01-01";

fn path(rest: &str) -> String {
    format!("{SUB}/resourceGroups/rg/providers/Microsoft.ServiceBus/namespaces/{rest}?{API}")
}

async fn put(client: &Client, rest: &str, body: Value) -> Reply {
    client.arm(Method::PUT, &path(rest), Some(body)).await
}

async fn get(client: &Client, rest: &str) -> Value {
    let reply = client.arm(Method::GET, &path(rest), None).await;
    assert_eq!(reply.status, StatusCode::OK, "{rest}: {}", reply.text());
    reply.json()
}

async fn namespace(client: &Client) {
    client.create_group("rg").await;
    let reply = put(client, "orders-bus", json!({"location": "eastus"})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
}

#[tokio::test]
async fn namespace_lifecycle() {
    let client = Client::new();
    client.create_group("rg").await;
    assert_eq!(put(&client, "short", json!({"location": "eastus"})).await.status, StatusCode::BAD_REQUEST);
    let reply = put(&client, "orders-bus", json!({"location": "eastus", "sku": {"name": "Gold"}})).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);

    let created = put(&client, "Orders-Bus", json!({"location": "eastus", "properties": {"status": "Hacked"}})).await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.text());
    let created = created.json();
    assert_eq!(created["sku"], json!({"name": "Standard", "tier": "Standard"}));
    assert_eq!(created["properties"]["status"], "Active", "service-owned properties cannot be set");
    assert_eq!(created["properties"]["provisioningState"], "Succeeded");
    assert_eq!(
        created["properties"]["serviceBusEndpoint"],
        "https://orders-bus.servicebus.azure.localhost:10443/"
    );

    // Names are global and case-insensitive.
    client.create_group("other").await;
    let clash = format!("{SUB}/resourceGroups/other/providers/Microsoft.ServiceBus/namespaces/orders-BUS?{API}");
    let reply = client.arm(Method::PUT, &clash, Some(json!({"location": "eastus"}))).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    let check = format!("{SUB}/providers/Microsoft.ServiceBus/checkNameAvailability?{API}");
    let reply = client.arm(Method::POST, &check, Some(json!({"name": "orders-bus"}))).await;
    assert_eq!(reply.json()["reason"], "NameInUse");
    let reply = client.arm(Method::POST, &check, Some(json!({"name": "another-bus"}))).await;
    assert_eq!(reply.json()["nameAvailable"], true);

    let premium = put(&client, "premium-bus", json!({"location": "eastus", "sku": {"name": "Premium"}})).await;
    assert_eq!(premium.json()["sku"]["capacity"], 1);

    let patched = client
        .arm(Method::PATCH, &path("orders-bus"), Some(json!({"tags": {"team": "orders"}, "properties": {"disableLocalAuth": true, "serviceBusEndpoint": "https://evil"}})))
        .await
        .json();
    assert_eq!(patched["tags"]["team"], "orders");
    assert_eq!(patched["properties"]["disableLocalAuth"], true);
    assert_eq!(patched["properties"]["serviceBusEndpoint"], created["properties"]["serviceBusEndpoint"]);

    // Deleting the namespace takes its children with it and frees the name.
    put(&client, "orders-bus/queues/incoming", json!({})).await;
    assert_eq!(client.arm(Method::DELETE, &path("orders-bus"), None).await.status, StatusCode::OK);
    let reply = client.arm(Method::GET, &path("orders-bus/queues/incoming"), None).await;
    assert_eq!(reply.error_code(), "ParentResourceNotFound");
    let reply = client.arm(Method::POST, &check, Some(json!({"name": "orders-bus"}))).await;
    assert_eq!(reply.json()["nameAvailable"], true);
}

#[tokio::test]
async fn authorization_rules_and_keys() {
    let client = Client::new();
    namespace(&client).await;

    // The built-in rule exists from the start.
    let rules = get(&client, "orders-bus/AuthorizationRules").await;
    assert_eq!(rules["value"][0]["name"], "RootManageSharedAccessKey");
    assert_eq!(rules["value"][0]["properties"]["rights"], json!(["Listen", "Manage", "Send"]));

    let keys_path = path("orders-bus/authorizationrules/RootManageSharedAccessKey/listKeys");
    let keys = client.arm(Method::POST, &keys_path, None).await.json();
    let primary = keys["primaryKey"].as_str().unwrap().to_string();
    assert_eq!(primary.len(), 44);
    assert_eq!(
        keys["primaryConnectionString"],
        format!("Endpoint=sb://orders-bus.servicebus.azure.localhost:10443/;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey={primary}")
    );
    // Keys are stable until regenerated, and only the named key changes.
    assert_eq!(client.arm(Method::POST, &keys_path, None).await.json()["primaryKey"], primary.as_str());
    let regenerate = path("orders-bus/AuthorizationRules/RootManageSharedAccessKey/regenerateKeys");
    let reply = client.arm(Method::POST, &regenerate, Some(json!({}))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let regenerated = client.arm(Method::POST, &regenerate, Some(json!({"keyType": "PrimaryKey"}))).await.json();
    assert_ne!(regenerated["primaryKey"], primary.as_str());
    assert_eq!(regenerated["secondaryKey"], keys["secondaryKey"]);

    let reply = put(&client, "orders-bus/AuthorizationRules/app", json!({"properties": {"rights": ["Manage"]}})).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "Manage needs Listen and Send");
    let reply = put(&client, "orders-bus/AuthorizationRules/app", json!({"properties": {"rights": ["Fly"]}})).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = put(&client, "orders-bus/AuthorizationRules/app", json!({"properties": {"rights": ["Send"]}})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());

    // Entity-level rules scope their connection strings to the entity.
    put(&client, "orders-bus/queues/incoming", json!({})).await;
    put(&client, "orders-bus/queues/incoming/authorizationRules/reader", json!({"properties": {"rights": ["Listen"]}})).await;
    let keys = client
        .arm(Method::POST, &path("orders-bus/queues/incoming/authorizationRules/reader/listKeys"), None)
        .await
        .json();
    assert!(keys["primaryConnectionString"].as_str().unwrap().ends_with(";EntityPath=incoming"));
}

#[tokio::test]
async fn queues_topics_subscriptions_and_rules() {
    let client = Client::new();
    namespace(&client).await;

    let queue = put(&client, "orders-bus/queues/incoming", json!({"properties": {"maxDeliveryCount": 3, "requiresSession": true, "messageCount": 99}})).await;
    assert_eq!(queue.status, StatusCode::OK, "{}", queue.text());
    let queue = queue.json();
    assert_eq!(queue["type"], "Microsoft.ServiceBus/namespaces/queues");
    assert_eq!(queue["properties"]["maxDeliveryCount"], 3);
    assert_eq!(queue["properties"]["lockDuration"], "PT1M");
    assert_eq!(queue["properties"]["defaultMessageTimeToLive"], "P10675199DT2H48M5.4775807S");
    assert_eq!(queue["properties"]["messageCount"], 0, "counts are the service's, not the caller's");
    assert_eq!(queue["properties"]["countDetails"]["activeMessageCount"], 0);

    // An update keeps what it does not mention, and some settings are fixed at creation.
    let updated = put(&client, "orders-bus/queues/incoming", json!({"properties": {"lockDuration": "PT2M"}})).await.json();
    assert_eq!(updated["properties"]["maxDeliveryCount"], 3);
    assert_eq!(updated["properties"]["createdAt"], queue["properties"]["createdAt"]);
    let reply = put(&client, "orders-bus/queues/incoming", json!({"properties": {"requiresSession": false}})).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = put(&client, "orders-bus/queues/bad", json!({"properties": {"maxDeliveryCount": 0}})).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);

    put(&client, "orders-bus/topics/events", json!({})).await;
    assert_eq!(get(&client, "orders-bus/topics/events").await["properties"]["subscriptionCount"], 0);
    let subscription = put(&client, "orders-bus/topics/events/subscriptions/audit", json!({"properties": {"maxDeliveryCount": 5}})).await;
    assert_eq!(subscription.status, StatusCode::OK, "{}", subscription.text());
    assert_eq!(subscription.json()["properties"]["deadLetteringOnFilterEvaluationExceptions"], true);
    put(&client, "orders-bus/topics/events/subscriptions/billing", json!({})).await;
    assert_eq!(get(&client, "orders-bus/topics/events").await["properties"]["subscriptionCount"], 2);
    let topics = get(&client, "orders-bus/topics").await;
    assert_eq!(topics["value"][0]["properties"]["subscriptionCount"], 2);

    // A subscription starts with a match-everything rule.
    let rules = get(&client, "orders-bus/topics/events/subscriptions/audit/rules").await;
    assert_eq!(rules["value"].as_array().unwrap().len(), 1);
    assert_eq!(rules["value"][0]["name"], "$Default");
    assert_eq!(rules["value"][0]["properties"]["sqlFilter"]["sqlExpression"], "1=1");

    let rule = "orders-bus/topics/events/subscriptions/audit/rules/high-value";
    let reply = put(&client, rule, json!({"properties": {"filterType": "SqlFilter", "sqlFilter": {"sqlExpression": "amount > 1000"}}})).await;
    assert_eq!(reply.json()["properties"]["sqlFilter"]["compatibilityLevel"], 20);
    let reply = put(&client, rule, json!({"properties": {"filterType": "CorrelationFilter", "correlationFilter": {"label": "vip"}}})).await;
    assert_eq!(reply.json()["properties"]["correlationFilter"]["label"], "vip");
    assert!(reply.json()["properties"].get("sqlFilter").is_none());
    let reply = put(&client, rule, json!({"properties": {"filterType": "CorrelationFilter"}})).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = put(&client, rule, json!({"properties": {"filterType": "Regex"}})).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);

    // Deleting a subscription removes its rules and lowers the count.
    let audit = path("orders-bus/topics/events/subscriptions/audit");
    assert_eq!(client.arm(Method::DELETE, &audit, None).await.status, StatusCode::OK);
    assert_eq!(get(&client, "orders-bus/topics/events").await["properties"]["subscriptionCount"], 1);
    let reply = client.arm(Method::GET, &path(rule), None).await;
    assert_eq!(reply.error_code(), "ParentResourceNotFound");
}
