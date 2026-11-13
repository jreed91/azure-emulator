//! Role definitions, role assignments, and enforcing them.

use azure_emulator::Config;
use http::{Method, StatusCode};
use serde_json::{json, Value};

use crate::support::{Client, SUB};

const API: &str = "api-version=2022-04-01";
const OWNER: &str = "8e3af657-a8ff-443c-a75c-2fe8c4bcb635";
const READER: &str = "acdd72a7-3385-48ef-bd42-f606fba81ae7";
const CONTRIBUTOR: &str = "b24988ac-6180-42a0-ab88-20f7382dd24c";

fn assignment_path(scope: &str, name: &str) -> String {
    format!("{scope}/providers/Microsoft.Authorization/roleAssignments/{name}?{API}")
}

fn assignment(role: &str, principal: &str) -> Value {
    json!({"properties": {
        "roleDefinitionId": format!("{SUB}/providers/Microsoft.Authorization/roleDefinitions/{role}"),
        "principalId": principal,
    }})
}

fn names(reply: &Value) -> Vec<String> {
    reply["value"].as_array().unwrap().iter().map(|v| v["name"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn built_in_role_definitions() {
    let client = Client::new();
    let reply = client
        .arm(Method::GET, &format!("{SUB}/providers/Microsoft.Authorization/roleDefinitions/{OWNER}?{API}"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let owner = reply.json();
    assert_eq!(owner["id"], format!("{SUB}/providers/Microsoft.Authorization/roleDefinitions/{OWNER}"));
    assert_eq!(owner["properties"]["roleName"], "Owner");
    assert_eq!(owner["properties"]["type"], "BuiltInRole");
    assert_eq!(owner["properties"]["permissions"][0]["actions"], json!(["*"]));

    let path = format!("{SUB}/providers/Microsoft.Authorization/roleDefinitions?{API}&$filter=roleName%20eq%20'Storage%20Blob%20Data%20Reader'");
    let reply = client.arm(Method::GET, &path, None).await;
    assert_eq!(names(&reply.json()), ["2a2b9908-6ea1-4ae2-8e65-a410df84e7d1"]);

    let reply = client.arm(Method::GET, &format!("{SUB}/providers/Microsoft.Authorization/roleDefinitions?{API}"), None).await;
    assert!(reply.json()["value"].as_array().unwrap().len() > 500);

    // Tenant-level ids work too.
    let reply = client
        .arm(Method::GET, &format!("/providers/Microsoft.Authorization/roleDefinitions/{READER}?{API}"), None)
        .await;
    assert_eq!(reply.json()["id"], format!("/providers/Microsoft.Authorization/roleDefinitions/{READER}"));
}

#[tokio::test]
async fn custom_roles() {
    let client = Client::new();
    client.create_group("roles").await;
    let id = "3a1f6d7e-0000-4000-8000-000000000001";
    let path = format!("{SUB}/providers/Microsoft.Authorization/roleDefinitions/{id}?{API}");
    let body = json!({"properties": {
        "roleName": "Blob Lister",
        "type": "CustomRole",
        "description": "Lists containers",
        "permissions": [{"actions": ["Microsoft.Storage/storageAccounts/blobServices/containers/read"], "notActions": []}],
        "assignableScopes": [format!("{SUB}/resourceGroups/roles")],
    }});
    let reply = client.arm(Method::PUT, &path, Some(body.clone())).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    assert_eq!(reply.json()["properties"]["type"], "CustomRole");

    // Names are unique, built-in ones included.
    let mut clash = body.clone();
    clash["properties"]["roleName"] = json!("Owner");
    let reply = client.arm(Method::PUT, &path.replace("0001", "0002"), Some(clash)).await;
    assert_eq!(reply.error_code(), "RoleDefinitionWithSameNameExists");

    let reply = client.arm(Method::GET, &format!("{SUB}/resourceGroups/roles/providers/Microsoft.Authorization/roleDefinitions?{API}&$filter=type%20eq%20'CustomRole'"), None).await;
    assert_eq!(names(&reply.json()), [id]);

    // Assignable only where it says.
    let group = format!("{SUB}/resourceGroups/roles");
    let reply = client.arm(Method::PUT, &assignment_path(&group, "aaaaaaaa-0000-4000-8000-000000000001"), Some(assignment(id, "someone"))).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    let reply = client.arm(Method::PUT, &assignment_path(SUB, "aaaaaaaa-0000-4000-8000-000000000002"), Some(assignment(id, "someone"))).await;
    assert_eq!(reply.error_code(), "RoleDefinitionDoesNotExist");

    // An assigned role cannot be deleted.
    let reply = client.arm(Method::DELETE, &path, None).await;
    assert_eq!(reply.error_code(), "RoleDefinitionHasAssignments");
    client.arm(Method::DELETE, &assignment_path(&group, "aaaaaaaa-0000-4000-8000-000000000001"), None).await;
    let reply = client.arm(Method::DELETE, &path, None).await;
    assert_eq!(reply.status, StatusCode::OK);

    let reply = client.arm(Method::DELETE, &format!("{SUB}/providers/Microsoft.Authorization/roleDefinitions/{OWNER}?{API}"), None).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn role_assignments_at_scopes() {
    let client = Client::new();
    client.create_group("ra").await;
    let account = format!("{SUB}/resourceGroups/ra/providers/Microsoft.Storage/storageAccounts/rastore");
    let reply = client
        .arm(Method::PUT, &format!("{account}?api-version=2023-01-01"), Some(json!({"location": "eastus", "sku": {"name": "Standard_LRS"}, "kind": "StorageV2"})))
        .await;
    assert!(reply.status.is_success());
    let group = format!("{SUB}/resourceGroups/ra");
    let at_sub = "11111111-aaaa-4000-8000-000000000001";
    let at_group = "11111111-aaaa-4000-8000-000000000002";
    let at_account = "11111111-aaaa-4000-8000-000000000003";

    let reply = client.arm(Method::PUT, &assignment_path(SUB, at_sub), Some(assignment(READER, "alice"))).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    let created = reply.json();
    assert_eq!(created["properties"]["scope"], SUB);
    assert_eq!(created["properties"]["roleDefinitionId"], format!("{SUB}/providers/Microsoft.Authorization/roleDefinitions/{READER}"));
    client.arm(Method::PUT, &assignment_path(&group, at_group), Some(assignment(CONTRIBUTOR, "bob"))).await;
    // Built-in roles may also be named by their tenant-level id.
    let mut body = assignment(OWNER, "carol");
    body["properties"]["roleDefinitionId"] = json!(format!("/providers/Microsoft.Authorization/roleDefinitions/{OWNER}"));
    let reply = client.arm(Method::PUT, &assignment_path(&account, at_account), Some(body)).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());

    // Listing at the group: what applies there (the subscription's) and below it.
    let reply = client.arm(Method::GET, &format!("{group}/providers/Microsoft.Authorization/roleAssignments?{API}"), None).await;
    let mut listed = names(&reply.json());
    listed.sort();
    assert_eq!(listed, [at_sub, at_group, at_account]);
    let reply = client
        .arm(Method::GET, &format!("{group}/providers/Microsoft.Authorization/roleAssignments?{API}&$filter=atScope()"), None)
        .await;
    let mut listed = names(&reply.json());
    listed.sort();
    assert_eq!(listed, [at_sub, at_group]);
    let reply = client
        .arm(Method::GET, &format!("{SUB}/providers/Microsoft.Authorization/roleAssignments?{API}&$filter=principalId%20eq%20'carol'"), None)
        .await;
    assert_eq!(names(&reply.json()), [at_account]);

    // The same role for the same principal at the same scope exists once.
    let reply = client.arm(Method::PUT, &assignment_path(SUB, "11111111-aaaa-4000-8000-000000000009"), Some(assignment(READER, "alice"))).await;
    assert_eq!(reply.error_code(), "RoleAssignmentExists");
    // An assignment's principal and role cannot change.
    let reply = client.arm(Method::PUT, &assignment_path(SUB, at_sub), Some(assignment(OWNER, "alice"))).await;
    assert_eq!(reply.error_code(), "RoleAssignmentUpdateNotPermitted");
    let reply = client.arm(Method::PUT, &assignment_path(SUB, "not-a-guid"), Some(assignment(READER, "dave"))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = client
        .arm(Method::PUT, &assignment_path(SUB, "11111111-aaaa-4000-8000-00000000000a"), Some(assignment("00000000-0000-0000-0000-00000000dead", "dave")))
        .await;
    assert_eq!(reply.error_code(), "RoleDefinitionDoesNotExist");
    let reply = client
        .arm(Method::PUT, &assignment_path(&format!("{SUB}/resourceGroups/nope"), "11111111-aaaa-4000-8000-00000000000b"), Some(assignment(READER, "dave")))
        .await;
    assert_eq!(reply.error_code(), "ResourceGroupNotFound");

    // Deleting a resource deletes the assignments on it.
    client.arm(Method::DELETE, &format!("{account}?api-version=2023-01-01"), None).await;
    let reply = client.arm(Method::GET, &assignment_path(&account, at_account), None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let reply = client.arm(Method::DELETE, &assignment_path(&group, at_group), None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["name"], at_group);
    let reply = client.arm(Method::GET, &assignment_path(&group, at_group), None).await;
    assert_eq!(reply.error_code(), "RoleAssignmentNotFound");
}

#[tokio::test]
async fn enforced_role_assignments() {
    let config = Config {
        enforce_rbac: true,
        rbac_owners: vec!["admin".into()],
        ..Config::default()
    };
    let client = Client::with_config(config);
    let group_path = |name: &str| format!("{SUB}/resourceGroups/{name}?api-version=2021-04-01");
    let reply = client.arm_as("admin", Method::PUT, &group_path("app"), Some(json!({"location": "eastus"}))).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    client.arm_as("admin", Method::PUT, &group_path("other"), Some(json!({"location": "eastus"}))).await;

    // With no assignment, nothing is allowed.
    let reply = client.arm_as("dev", Method::GET, &group_path("app"), None).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.error_code(), "AuthorizationFailed");
    assert!(reply.json()["error"]["message"].as_str().unwrap().contains("Microsoft.Resources/subscriptions/resourceGroups/read"));

    // Contributor on one group: manage resources there, not elsewhere, and not access.
    let group = format!("{SUB}/resourceGroups/app");
    let reply = client
        .arm_as("admin", Method::PUT, &assignment_path(&group, "22222222-aaaa-4000-8000-000000000001"), Some(assignment(CONTRIBUTOR, "dev")))
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    let identity = |group: &str, name: &str| {
        format!("{SUB}/resourceGroups/{group}/providers/Microsoft.ManagedIdentity/userAssignedIdentities/{name}?api-version=2023-01-31")
    };
    let reply = client.arm_as("dev", Method::PUT, &identity("app", "worker"), Some(json!({"location": "eastus"}))).await;
    assert!(reply.status.is_success(), "{}", reply.text());
    let reply = client.arm_as("dev", Method::PUT, &identity("other", "worker"), Some(json!({"location": "eastus"}))).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = client
        .arm_as("dev", Method::PUT, &assignment_path(&group, "22222222-aaaa-4000-8000-000000000002"), Some(assignment(OWNER, "dev")))
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(reply.json()["error"]["message"].as_str().unwrap().contains("Microsoft.Authorization/roleAssignments/write"));

    // A template deploys only what its caller may create.
    let template = json!({
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "resources": [{
            "type": "Microsoft.Authorization/roleAssignments", "apiVersion": "2022-04-01",
            "name": "22222222-aaaa-4000-8000-000000000003",
            "properties": {"roleDefinitionId": format!("{SUB}/providers/Microsoft.Authorization/roleDefinitions/{OWNER}"), "principalId": "dev"},
        }],
    });
    let reply = client
        .arm_as(
            "dev",
            Method::PUT,
            &format!("{group}/providers/Microsoft.Resources/deployments/escalate?api-version=2022-09-01"),
            Some(json!({"properties": {"mode": "Incremental", "template": template}})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    assert_eq!(reply.json()["properties"]["error"]["details"][0]["code"], "AuthorizationFailed");

    // A managed identity's token acts as its principal.
    let reply = client.arm_as("dev", Method::GET, &identity("app", "worker"), None).await;
    let worker = reply.json();
    let principal = worker["properties"]["principalId"].as_str().unwrap();
    let client_id = worker["properties"]["clientId"].as_str().unwrap();
    let reply = client.arm_as(client_id, Method::GET, &group_path("app"), None).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    client
        .arm_as("admin", Method::PUT, &assignment_path(&group, "22222222-aaaa-4000-8000-000000000004"), Some(assignment(READER, principal)))
        .await;
    let reply = client.arm_as(client_id, Method::GET, &group_path("app"), None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let reply = client.arm_as(client_id, Method::DELETE, &group_path("app"), None).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
}
