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

    let reply = client
        .arm(Method::PATCH, &path("orders-bus/queues/incoming"), Some(json!({"properties": {"messageCount": 99, "maxDeliveryCount": 7}})))
        .await
        .json();
    assert_eq!(reply["properties"]["messageCount"], 0, "an update cannot set service-owned properties");
    assert_eq!(reply["properties"]["maxDeliveryCount"], 7);

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

#[tokio::test]
async fn network_rule_set() {
    let client = Client::new();
    namespace(&client).await;
    let rules = get(&client, "orders-bus/networkRuleSets/default").await;
    assert_eq!(rules["properties"]["defaultAction"], "Allow");
    assert_eq!(rules["properties"]["ipRules"], json!([]));

    let update = json!({"properties": {"defaultAction": "Deny", "ipRules": [{"ipMask": "10.0.0.0/24", "action": "Allow"}]}});
    let reply = put(&client, "orders-bus/networkRuleSets/default", update).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["properties"]["ipRules"][0]["ipMask"], "10.0.0.0/24");
    assert_eq!(reply.json()["properties"]["publicNetworkAccess"], "Enabled", "unmentioned settings are kept");
    assert_eq!(get(&client, "orders-bus/networkRuleSets").await["value"].as_array().unwrap().len(), 1);

    let reply = put(&client, "orders-bus/networkRuleSets/default", json!({"properties": {"defaultAction": "Maybe"}})).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = put(&client, "orders-bus/networkRuleSets/other", json!({"properties": {}})).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn geo_disaster_recovery_alias() {
    let client = Client::new();
    client.create_group("rg").await;
    let premium = json!({"location": "eastus", "sku": {"name": "Premium"}});
    for name in ["primary-bus", "secondary-bus"] {
        assert_eq!(put(&client, name, premium.clone()).await.status, StatusCode::OK);
    }
    put(&client, "standard-bus", json!({"location": "eastus"})).await;
    let secondary_id = format!("{SUB}/resourceGroups/rg/providers/Microsoft.ServiceBus/namespaces/secondary-bus");
    let pair = json!({"properties": {"partnerNamespace": secondary_id}});

    // Pairing needs two different Premium namespaces that exist.
    assert_eq!(put(&client, "standard-bus/disasterRecoveryConfigs/orders-alias", pair.clone()).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(put(&client, "secondary-bus/disasterRecoveryConfigs/orders-alias", pair.clone()).await.status, StatusCode::BAD_REQUEST);
    let ghost = json!({"properties": {"partnerNamespace": secondary_id.replace("secondary-bus", "ghost-bus")}});
    assert_eq!(put(&client, "primary-bus/disasterRecoveryConfigs/orders-alias", ghost).await.status, StatusCode::BAD_REQUEST);

    let reply = put(&client, "primary-bus/disasterRecoveryConfigs/orders-alias", pair.clone()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["properties"]["role"], "Primary");
    // The partner sees the same alias from its side.
    let mirror = get(&client, "secondary-bus/disasterRecoveryConfigs/orders-alias").await;
    assert_eq!(mirror["properties"]["role"], "Secondary");
    assert!(mirror["properties"]["partnerNamespace"].as_str().unwrap().ends_with("/namespaces/primary-bus"));

    // The alias is a name in the namespace name space.
    let check = path("primary-bus/disasterRecoveryConfigs/CheckNameAvailability");
    let reply = client.arm(Method::POST, &check, Some(json!({"name": "orders-alias"}))).await;
    assert_eq!(reply.json()["reason"], "NameInUse", "{}", reply.text());
    assert_eq!(put(&client, "orders-alias", premium.clone()).await.status, StatusCode::CONFLICT);

    // The alias exposes the namespace's rules, with alias connection strings.
    let rules = get(&client, "primary-bus/disasterRecoveryConfigs/orders-alias/authorizationRules").await;
    assert_eq!(rules["value"][0]["name"], "RootManageSharedAccessKey");
    let keys = client
        .arm(Method::POST, &path("primary-bus/disasterRecoveryConfigs/orders-alias/authorizationRules/RootManageSharedAccessKey/listKeys"), None)
        .await
        .json();
    assert!(keys["aliasPrimaryConnectionString"].as_str().unwrap().starts_with("Endpoint=sb://orders-alias.servicebus."));
    let direct = client
        .arm(Method::POST, &path("primary-bus/AuthorizationRules/RootManageSharedAccessKey/listKeys"), None)
        .await
        .json();
    assert_eq!(keys["primaryKey"], direct["primaryKey"], "the alias uses the namespace's keys");

    // Failover is the secondary's call; afterwards it holds the alias alone.
    let failover = path("primary-bus/disasterRecoveryConfigs/orders-alias/failover");
    assert_eq!(client.arm(Method::POST, &failover, None).await.status, StatusCode::BAD_REQUEST);
    let failover = path("secondary-bus/disasterRecoveryConfigs/orders-alias/failover");
    let reply = client.arm(Method::POST, &failover, None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert!(reply.body.is_empty());
    let survivor = get(&client, "secondary-bus/disasterRecoveryConfigs/orders-alias").await;
    assert_eq!(survivor["properties"]["role"], "PrimaryNotReplicating");
    assert_eq!(survivor["properties"]["partnerNamespace"], "");
    let reply = client.arm(Method::GET, &path("primary-bus/disasterRecoveryConfigs/orders-alias"), None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    // Deleting the last holder frees the name; a new pairing can then be broken.
    assert_eq!(client.arm(Method::DELETE, &path("secondary-bus/disasterRecoveryConfigs/orders-alias"), None).await.status, StatusCode::OK);
    let reply = client.arm(Method::POST, &check, Some(json!({"name": "orders-alias"}))).await;
    assert_eq!(reply.json()["nameAvailable"], true);
    put(&client, "primary-bus/disasterRecoveryConfigs/orders-alias", pair).await;
    let reply = client.arm(Method::POST, &path("primary-bus/disasterRecoveryConfigs/orders-alias/breakPairing"), None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(get(&client, "primary-bus/disasterRecoveryConfigs/orders-alias").await["properties"]["role"], "PrimaryNotReplicating");
    let reply = client.arm(Method::GET, &path("secondary-bus/disasterRecoveryConfigs/orders-alias"), None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn legacy_filter_rules_and_private_link_resources() {
    let client = Client::new();
    namespace(&client).await;

    // The legacy rules exist only in the api-version that introduced them.
    let legacy = |rest: &str| {
        format!("{SUB}/resourceGroups/rg/providers/Microsoft.ServiceBus/namespaces/{rest}?api-version=2018-01-01-preview")
    };
    let put_legacy = |rest: &str, body: Value| {
        let path = legacy(rest);
        let client = &client;
        async move { client.arm(Method::PUT, &path, Some(body)).await }
    };
    let reply = put(&client, "orders-bus/ipfilterrules/office", json!({"properties": {"ipMask": "203.0.113.0/24"}})).await;
    assert_eq!(reply.error_code(), "NoRegisteredProviderFound");
    assert_eq!(put_legacy("orders-bus/ipfilterrules/office", json!({"properties": {}})).await.status, StatusCode::BAD_REQUEST);
    let reply = put_legacy("orders-bus/ipfilterrules/office", json!({"properties": {"ipMask": "203.0.113.0/24"}})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["properties"]["action"], "Accept");
    assert_eq!(reply.json()["properties"]["filterName"], "office");
    let reply = put_legacy("orders-bus/ipfilterrules/office", json!({"properties": {"ipMask": "203.0.113.0/24", "action": "Drop"}})).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);

    let subnet = "/subscriptions/x/resourceGroups/net/providers/Microsoft.Network/virtualNetworks/v/subnets/s";
    let reply = put_legacy("orders-bus/virtualnetworkrules/app", json!({"properties": {"virtualNetworkSubnetId": subnet}})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["properties"]["virtualNetworkSubnetId"], subnet);
    let listed = client.arm(Method::GET, &legacy("orders-bus/virtualnetworkrules"), None).await.json();
    assert_eq!(listed["value"].as_array().unwrap().len(), 1);

    let links = get(&client, "orders-bus/privateLinkResources").await;
    assert_eq!(links["value"][0]["properties"]["groupId"], "namespace");
    assert_eq!(links["value"][0]["properties"]["requiredZoneNames"], json!(["privatelink.servicebus.windows.net"]));
}

#[tokio::test]
async fn post_forms_of_rule_reads_and_legacy_event_hubs() {
    let client = Client::new();
    namespace(&client).await;
    put(&client, "orders-bus/queues/incoming", json!({})).await;
    put(&client, "orders-bus/queues/incoming/authorizationRules/reader", json!({"properties": {"rights": ["Listen"]}})).await;

    let listed = client.arm(Method::POST, &path("orders-bus/AuthorizationRules"), None).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.text());
    assert_eq!(listed.json()["value"][0]["name"], "RootManageSharedAccessKey");
    let one = client.arm(Method::POST, &path("orders-bus/AuthorizationRules/RootManageSharedAccessKey"), None).await;
    assert_eq!(one.json()["properties"]["rights"], json!(["Listen", "Manage", "Send"]));
    let entity = client.arm(Method::POST, &path("orders-bus/queues/incoming/authorizationRules/reader"), None).await;
    assert_eq!(entity.json()["name"], "reader");
    let missing = client.arm(Method::POST, &path("orders-bus/topics/none/authorizationRules/reader"), None).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    // Event hubs in Service Bus namespaces exist only in the older api-versions.
    let legacy = |rest: &str| {
        format!("{SUB}/resourceGroups/rg/providers/Microsoft.ServiceBus/namespaces/{rest}?api-version=2017-04-01")
    };
    let hubs = client.arm(Method::GET, &legacy("orders-bus/eventhubs"), None).await;
    assert_eq!(hubs.status, StatusCode::OK, "{}", hubs.text());
    assert_eq!(hubs.json(), json!({"value": []}));
    // ... and their rules only in the oldest one.
    let rules = legacy("orders-bus/eventhubs/hub/authorizationRules/r").replace("2017-04-01", "2014-09-01");
    let reply = client.arm(Method::GET, &rules, None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.text());
}

#[tokio::test]
async fn standard_to_premium_migration() {
    let client = Client::new();
    client.create_group("rg").await;
    put(&client, "standard-bus", json!({"location": "eastus"})).await;
    put(&client, "premium-bus", json!({"location": "eastus", "sku": {"name": "Premium"}})).await;
    put(&client, "standard-bus/queues/jobs", json!({"properties": {"maxDeliveryCount": 3}})).await;
    put(&client, "standard-bus/topics/events", json!({})).await;
    put(&client, "standard-bus/topics/events/subscriptions/audit", json!({})).await;
    let premium_id = format!("{SUB}/resourceGroups/rg/providers/Microsoft.ServiceBus/namespaces/premium-bus");
    let config = json!({"properties": {"targetNamespace": premium_id, "postMigrationName": "standard-bus-old"}});

    assert_eq!(put(&client, "standard-bus/migrationConfigurations/other", config.clone()).await.status, StatusCode::BAD_REQUEST);
    let wrong_way = json!({"properties": {"targetNamespace": premium_id.replace("premium-bus", "standard-bus"), "postMigrationName": "x-old-name"}});
    assert_eq!(put(&client, "standard-bus/migrationConfigurations/$default", wrong_way).await.status, StatusCode::BAD_REQUEST);

    let reply = put(&client, "standard-bus/migrationConfigurations/$default", config.clone()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["properties"]["migrationState"], "Active");

    // The entities' metadata now exists on the Premium namespace too.
    let queue = get(&client, "premium-bus/queues/jobs").await;
    assert_eq!(queue["properties"]["maxDeliveryCount"], 3);
    assert!(queue["id"].as_str().unwrap().contains("/namespaces/premium-bus/"));
    assert_eq!(get(&client, "premium-bus/topics/events/subscriptions/audit/rules").await["value"][0]["name"], "$Default");
    assert_eq!(get(&client, "standard-bus/migrationConfigurations").await["value"].as_array().unwrap().len(), 1);
    assert_eq!(put(&client, "standard-bus/migrationConfigurations/$default", config).await.status, StatusCode::CONFLICT);

    // Completing the migration is not modeled; abandoning it is.
    let upgrade = client.arm(Method::POST, &path("standard-bus/migrationConfigurations/$default/upgrade"), None).await;
    assert_eq!(upgrade.status, StatusCode::NOT_IMPLEMENTED);
    let revert = client.arm(Method::POST, &path("standard-bus/migrationConfigurations/$default/revert"), None).await;
    assert_eq!(revert.status, StatusCode::OK);
    assert_eq!(get(&client, "standard-bus/migrationConfigurations").await, json!({"value": []}));
}
