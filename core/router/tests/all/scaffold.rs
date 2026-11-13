use http::{Method, StatusCode};
use serde_json::json;

use crate::support::{Client, SUB};

/// A type that is modeled but not implemented.
const NAMESPACES: &str = "/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/acct/objectReplicationPolicies";

#[tokio::test]
async fn scaffolded_types_are_mounted_as_honest_stubs() {
    let client = Client::new();
    client.create_group("rg").await;
    let account = format!("{SUB}/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/acct?api-version=2024-01-01");
    let body = json!({"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}});
    assert_eq!(client.arm(Method::PUT, &account, Some(body)).await.status, StatusCode::OK);
    let item = format!("{SUB}{NAMESPACES}/demo?api-version=2024-01-01");

    // Creating is the part nobody has written yet.
    let reply = client.arm(Method::PUT, &item, Some(json!({"properties": {}}))).await;
    assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED, "{}", reply.text());
    assert_eq!(reply.error_code(), "NotImplemented");

    // Nothing can exist, so reads say so truthfully.
    let reply = client.arm(Method::GET, &item, None).await;
    assert_eq!(reply.error_code(), "ResourceNotFound");
    let reply = client.arm(Method::GET, &format!("{SUB}{NAMESPACES}?api-version=2024-01-01"), None).await;
    assert_eq!(reply.json(), json!({"value": []}));

    // The provider is known to Resource Manager, with its types.
    let reply = client.arm(Method::GET, &format!("{SUB}/providers/Microsoft.ServiceBus?api-version=2021-04-01"), None).await;
    let types = reply.json()["resourceTypes"].to_string();
    assert!(types.contains("namespaces/queues"), "{types}");
}

#[tokio::test]
async fn api_versions_come_from_the_specification() {
    let client = Client::new();
    client.create_group("rg").await;
    let reply = client
        .arm(Method::GET, &format!("{SUB}/resourceGroups/rg/providers/Microsoft.ServiceBus/namespaces?api-version=1999-01-01"), None)
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.error_code(), "NoRegisteredProviderFound");
    assert!(reply.json()["error"]["message"].as_str().unwrap().contains("2024-01-01"));

    // Hand-written types are checked against the same table.
    let account = format!("{SUB}/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/abc?api-version=1999-01-01");
    assert_eq!(client.arm(Method::GET, &account, None).await.error_code(), "NoRegisteredProviderFound");
}

#[tokio::test]
async fn coverage_separates_implemented_from_stubbed() {
    let coverage = Client::new().emulator.coverage();
    let implemented = |id: &str| {
        coverage
            .iter()
            .find(|c| c.operation.id == id)
            .unwrap_or_else(|| panic!("{id} is not in any scaffold"))
            .implemented
    };
    assert!(implemented("StorageAccounts_Create"));
    assert!(implemented("StorageAccounts_ListKeys"));
    assert!(implemented("StorageAccounts_CheckNameAvailability"));
    assert!(implemented("Vaults_CreateOrUpdate"));
    assert!(!implemented("StorageAccounts_Failover"), "an action nobody wrote");
    assert!(!implemented("ObjectReplicationPolicies_CreateOrUpdate"), "a stubbed nested type");
    assert!(!implemented("MigrationConfigs_CreateAndStartMigration"), "a stubbed type");
    assert!(!implemented("Operations_List"), "not routed yet");
}

#[tokio::test]
async fn implementing_a_scaffolded_type_only_takes_a_handler() {
    let client = Client::new();
    client.create_group("rg").await;
    let path = format!(
        "{SUB}/resourceGroups/rg/providers/Microsoft.ManagedIdentity/userAssignedIdentities/app-identity?api-version=2024-11-30"
    );
    let created = client.arm(Method::PUT, &path, Some(json!({"location": "eastus"}))).await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.text());
    let created = created.json();
    assert_eq!(created["type"], "Microsoft.ManagedIdentity/userAssignedIdentities");
    assert_eq!(created["properties"]["tenantId"], client.config().tenant_id);
    let client_id = created["properties"]["clientId"].as_str().unwrap().to_string();
    assert_eq!(client_id.len(), 36);

    // Identifiers survive updates, and a caller cannot overwrite them.
    let body = json!({"location": "eastus", "tags": {"a": "b"}, "properties": {"clientId": "forged"}});
    let updated = client.arm(Method::PUT, &path, Some(body)).await.json();
    assert_eq!(updated["properties"]["clientId"], client_id.as_str());
    let patched = client
        .arm(Method::PATCH, &path, Some(json!({"properties": {"principalId": "forged"}})))
        .await
        .json();
    assert_eq!(patched["properties"]["principalId"], created["properties"]["principalId"]);
    assert_eq!(client.arm(Method::HEAD, &path, None).await.status, StatusCode::NO_CONTENT);

    // Its child type validates what the specification requires.
    let child = path.replace("?", "/federatedIdentityCredentials/github?");
    let reply = client.arm(Method::PUT, &child, Some(json!({"properties": {}}))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let body = json!({"properties": {
        "issuer": "https://token.actions.githubusercontent.com",
        "subject": "repo:octo/app:ref:refs/heads/main",
        "audiences": ["api://AzureADTokenExchange"],
    }});
    let reply = client.arm(Method::PUT, &child, Some(body)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["name"], "github");
    assert!(reply.json().get("location").is_none(), "child resources have no location");
    let listing = client.arm(Method::GET, &path.replace("?", "/federatedIdentityCredentials?"), None).await;
    assert_eq!(listing.json()["value"].as_array().unwrap().len(), 1);
    let orphan = path.replace("app-identity?", "missing/federatedIdentityCredentials/github?");
    assert_eq!(client.arm(Method::GET, &orphan, None).await.error_code(), "ParentResourceNotFound");

    let coverage = client.emulator.coverage();
    let done = |id: &str| coverage.iter().find(|c| c.operation.id == id).unwrap().implemented;
    assert!(done("UserAssignedIdentities_CreateOrUpdate"));
    assert!(!done("UserAssignedIdentities_RevokeTokens"));
    assert!(done("FederatedIdentityCredentials_CreateOrUpdate"));
}

#[tokio::test]
async fn resources_only_microsoft_network_creates_never_exist() {
    let client = Client::new();
    client.create_group("rg").await;
    let account = format!("{SUB}/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/acct");
    let body = json!({"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}});
    client.arm(Method::PUT, &format!("{account}?api-version=2024-01-01"), Some(body)).await;

    for collection in ["privateEndpointConnections", "networkSecurityPerimeterConfigurations"] {
        let list = format!("{account}/{collection}?api-version=2024-01-01");
        let reply = client.arm(Method::GET, &list, None).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        assert_eq!(reply.json(), json!({"value": []}));
        let one = list.replace("?", "/some-connection?");
        assert_eq!(client.arm(Method::GET, &one, None).await.status, StatusCode::NOT_FOUND);
    }
    // Approving a connection that cannot exist is a 404, never a made-up success.
    let connection = format!("{account}/privateEndpointConnections/pe1?api-version=2024-01-01");
    let approve = json!({"properties": {"privateLinkServiceConnectionState": {"status": "Approved"}}});
    let reply = client.arm(Method::PUT, &connection, Some(approve)).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(reply.json()["error"]["message"].as_str().unwrap().contains("Microsoft.Network"));
    let reconcile = format!("{account}/networkSecurityPerimeterConfigurations/nsp1/reconcile?api-version=2024-01-01");
    assert_eq!(client.arm(Method::POST, &reconcile, None).await.status, StatusCode::NOT_FOUND);
}
