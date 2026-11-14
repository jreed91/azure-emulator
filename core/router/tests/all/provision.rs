//! Provisioning from a configuration file (`--config`): each entry becomes a
//! Resource Manager request, so what it makes is checked through Resource
//! Manager, and a refused request stops provisioning with the reason.

use azure_emulator::provision::provision;
use http::Method;
use serde_json::{json, Value};

use crate::support::{Client, SUB};

async fn get(client: &Client, path: &str, api: &str) -> Value {
    let reply = client.arm(Method::GET, &format!("{path}?api-version={api}"), None).await;
    assert!(reply.status.is_success(), "{path}: {}", reply.text());
    reply.json()
}

async fn post(client: &Client, path: &str, api: &str) -> Value {
    let reply = client.arm(Method::POST, &format!("{path}?api-version={api}"), None).await;
    assert!(reply.status.is_success(), "{path}: {}", reply.text());
    reply.json()
}

#[tokio::test]
async fn event_hubs_cosmos_and_raw_resources_are_provisioned() {
    let client = Client::new();
    let primary = "cHJpbWFyeS1rZXktZm9yLXRoZS1ldmVudC1odWJzLW5z";
    let secondary = "c2Vjb25kYXJ5LWtleS1mb3ItdGhlLWV2ZW50LWh1YnM=";
    let cosmos_key = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";
    let file = json!({
        "eventHubs": [{
            "resourceGroup": "provisioned",
            "namespace": "provisioned-hubs",
            "keys": {
                "RootManageSharedAccessKey": {"primary": primary, "secondary": secondary},
                "sender": primary,
            },
            "hubs": [{"name": "telemetry", "partitionCount": 2, "messageRetentionInDays": 3, "consumerGroups": ["$Default", "analytics"]}],
        }],
        "cosmos": [{
            "resourceGroup": "provisioned",
            "account": "provisioned-cosmos",
            "key": cosmos_key,
            "databases": [{"id": "app", "throughput": 400, "containers": [{"id": "orders", "partitionKey": "/tenant", "throughput": 500}]}],
        }],
        "resources": [{
            "id": format!("{SUB}/resourceGroups/provisioned/providers/Microsoft.ManagedIdentity/userAssignedIdentities/worker"),
            "apiVersion": "2023-01-31",
            "body": {"location": "eastus", "tags": {"from": "config"}},
        }],
    });
    let done = provision(&client.emulator, &file).await.expect("provisioning succeeds");
    assert!(done.iter().any(|line| line.ends_with("/consumergroups/analytics")));
    assert!(!done.iter().any(|line| line.contains("$Default")), "the default consumer group is not created again");

    let group = format!("{SUB}/resourceGroups/provisioned/providers");
    let namespace = format!("{group}/Microsoft.EventHub/namespaces/provisioned-hubs");
    let hub = get(&client, &format!("{namespace}/eventhubs/telemetry"), "2024-01-01").await;
    assert_eq!((&hub["properties"]["partitionCount"], &hub["properties"]["messageRetentionInDays"]), (&json!(2), &json!(3)));
    let groups = get(&client, &format!("{namespace}/eventhubs/telemetry/consumergroups"), "2024-01-01").await;
    let mut names: Vec<&str> = groups["value"].as_array().unwrap().iter().map(|g| g["name"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["$Default", "analytics"]);
    let keys = post(&client, &format!("{namespace}/authorizationRules/RootManageSharedAccessKey/listKeys"), "2024-01-01").await;
    assert_eq!((keys["primaryKey"].as_str(), keys["secondaryKey"].as_str()), (Some(primary), Some(secondary)));
    let sender = post(&client, &format!("{namespace}/authorizationRules/sender/listKeys"), "2024-01-01").await;
    assert_eq!(sender["primaryKey"], primary);

    let account = format!("{group}/Microsoft.DocumentDB/databaseAccounts/provisioned-cosmos");
    let cosmos_keys = post(&client, &format!("{account}/listKeys"), "2024-11-15").await;
    assert_eq!(cosmos_keys["primaryMasterKey"], cosmos_key);
    let database = get(&client, &format!("{account}/sqlDatabases/app"), "2024-11-15").await;
    assert_eq!(database["properties"]["options"]["throughput"], 400);
    let container = get(&client, &format!("{account}/sqlDatabases/app/containers/orders"), "2024-11-15").await;
    assert_eq!(container["properties"]["resource"]["partitionKey"]["paths"], json!(["/tenant"]));
    assert_eq!(container["properties"]["options"]["throughput"], 500);

    let identity = get(&client, &format!("{group}/Microsoft.ManagedIdentity/userAssignedIdentities/worker"), "2023-01-31").await;
    assert_eq!(identity["tags"], json!({"from": "config"}));

    // Provisioning again updates what is there rather than failing.
    provision(&client.emulator, &file).await.expect("provisioning is repeatable");
}

#[tokio::test]
async fn bad_files_and_refused_requests_stop_provisioning() {
    let client = Client::new();
    let cases = [
        (json!({"eventHubs": [{"resourceGroup": "g", "namespace": "bad-keys-ns", "keys": {"RootManageSharedAccessKey": 5}}]}), "keys.RootManageSharedAccessKey"),
        (json!({"serviceBus": [{"resourceGroup": "g", "namespace": "bad-keys-sb", "keys": {"app": ["k"]}}]}), "keys.app"),
        (json!({"eventHubs": [{"resourceGroup": "g", "namespace": "cg-names", "hubs": [{"name": "h", "consumerGroups": [1]}]}]}), "consumer groups are names"),
        (json!({"resources": [{"id": format!("{SUB}/resourceGroups/g")}]}), "apiVersion"),
        (json!({"cosmos": [{"resourceGroup": "g", "account": "c", "databases": [{"containers": []}]}]}), "'id' is required"),
        // A request the emulator refuses: an invalid namespace name.
        (json!({"serviceBus": [{"resourceGroup": "g", "namespace": "x"}]}), "400"),
    ];
    for (file, fragment) in cases {
        let error = provision(&client.emulator, &file).await.expect_err("provisioning fails");
        assert!(error.contains(fragment), "expected '{fragment}' in: {error}");
    }
}
