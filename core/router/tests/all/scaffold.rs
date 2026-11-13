use http::{Method, StatusCode};
use serde_json::{json, Value};

use crate::support::{Client, SUB};

/// A type that is modeled but not implemented.
const NAMESPACES: &str = "/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/acct/storageTaskAssignments";

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
    assert!(!implemented("StorageTaskAssignments_Create"), "a stubbed nested type");
    assert!(!implemented("Connectors_Create"), "a stubbed type");
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

#[tokio::test]
async fn managed_identities_on_resources() {
    let client = Client::new();
    client.create_group("rg").await;
    let uai = |name: &str| format!("{SUB}/resourceGroups/rg/providers/Microsoft.ManagedIdentity/userAssignedIdentities/{name}");
    for name in ["app", "jobs"] {
        client.arm(Method::PUT, &format!("{}?api-version=2024-11-30", uai(name)), Some(json!({"location": "eastus"}))).await;
    }
    let app = client.arm(Method::GET, &format!("{}?api-version=2024-11-30", uai("app")), None).await.json();
    let account = format!("{SUB}/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/idacct");
    let at = |path: &str| format!("{path}?api-version=2024-01-01");
    let body = |identity: Value| json!({"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}, "identity": identity});

    // A system-assigned identity gets ids, kept across updates.
    let created = client.arm(Method::PUT, &at(&account), Some(body(json!({"type": "SystemAssigned"})))).await.json();
    let principal = created["identity"]["principalId"].as_str().unwrap().to_string();
    assert_eq!(principal.len(), 36);
    assert_eq!(created["identity"]["tenantId"], client.config().tenant_id);
    let both = json!({"type": "SystemAssigned,UserAssigned", "userAssignedIdentities": {uai("app"): {}}});
    let updated = client.arm(Method::PUT, &at(&account), Some(body(both))).await.json();
    assert_eq!(updated["identity"]["type"], "SystemAssigned, UserAssigned");
    assert_eq!(updated["identity"]["principalId"], principal.as_str());
    let attached = &updated["identity"]["userAssignedIdentities"][uai("app")];
    assert_eq!(attached["clientId"], app["properties"]["clientId"]);

    // A PATCH adds identities, and null removes one.
    let patched = client
        .arm(Method::PATCH, &at(&account), Some(json!({"identity": {"type": "SystemAssigned,UserAssigned", "userAssignedIdentities": {uai("jobs"): {}}}})))
        .await
        .json();
    assert_eq!(patched["identity"]["userAssignedIdentities"].as_object().unwrap().len(), 2);
    let patched = client
        .arm(Method::PATCH, &at(&account), Some(json!({"identity": {"type": "SystemAssigned,UserAssigned", "userAssignedIdentities": {uai("jobs"): null}}})))
        .await
        .json();
    assert_eq!(patched["identity"]["userAssignedIdentities"].as_object().unwrap().len(), 1);

    // The system-assigned identity can be read by the resource's id.
    let by_scope = format!("{account}/providers/Microsoft.ManagedIdentity/identities/default?api-version=2024-11-30");
    let reply = client.arm(Method::GET, &by_scope, None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["properties"]["principalId"], principal.as_str());
    assert_eq!(reply.json()["properties"]["clientId"].as_str().unwrap().len(), 36);

    // The user-assigned identity knows who uses it.
    let associated = format!("{}/listAssociatedResources?api-version=2024-11-30", uai("app"));
    let reply = client.arm(Method::POST, &associated, None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["totalCount"], 1);
    assert_eq!(reply.json()["value"][0]["id"], account.as_str());

    // Refusals: unknown identities and types.
    let ghost = json!({"type": "UserAssigned", "userAssignedIdentities": {uai("ghost"): {}}});
    let reply = client.arm(Method::PUT, &at(&format!("{account}x")), Some(body(ghost))).await;
    assert_eq!(reply.error_code(), "InvalidIdentity");
    let reply = client.arm(Method::PUT, &at(&format!("{account}x")), Some(body(json!({"type": "Magic"})))).await;
    assert_eq!(reply.error_code(), "InvalidIdentity");
    let check = format!("{SUB}/providers/Microsoft.Storage/checkNameAvailability?api-version=2024-01-01");
    let reply = client.arm(Method::POST, &check, Some(json!({"name": "idacctx"}))).await;
    assert_eq!(reply.json()["nameAvailable"], true, "a refused PUT leaves nothing behind");

    // Turning the system identity off and on again gives a new principal.
    client.arm(Method::PUT, &at(&account), Some(body(json!({"type": "None"})))).await;
    assert_eq!(client.arm(Method::GET, &by_scope, None).await.status, StatusCode::NOT_FOUND);
    let again = client.arm(Method::PUT, &at(&account), Some(body(json!({"type": "SystemAssigned"})))).await.json();
    assert_ne!(again["identity"]["principalId"], principal.as_str());
}
