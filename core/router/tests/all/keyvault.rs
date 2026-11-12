use http::{Method, StatusCode};
use serde_json::{json, Value};

use crate::support::{Client, Reply, SUB};

const HOST: &str = "demo-vault.vault.azure.localhost:10443";
const VAULT_SCOPE: &str = "https://vault.azure.localhost:10443/.default";

fn vault_path(name: &str) -> String {
    format!("{SUB}/resourceGroups/rg/providers/Microsoft.KeyVault/vaults/{name}?api-version=2023-07-01")
}

async fn vault_client() -> (Client, String) {
    let client = Client::new();
    client.create_group("rg").await;
    let body = json!({
        "location": "eastus",
        "properties": {
            "tenantId": client.config().tenant_id,
            "sku": {"family": "A", "name": "standard"},
            "enableRbacAuthorization": true,
        },
    });
    let reply = client.arm(Method::PUT, &vault_path("Demo-Vault"), Some(body)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        reply.json()["properties"]["vaultUri"],
        "https://demo-vault.vault.azure.localhost:10443/"
    );
    let authorization = format!("Bearer {}", client.token(VAULT_SCOPE).await);
    (client, authorization)
}

async fn call(client: &Client, authorization: &str, method: Method, path: &str, body: Option<Value>) -> Reply {
    let path = format!("{path}?api-version=7.4");
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    client
        .send(method, HOST, &path, &[("authorization", authorization), ("content-type", "application/json")], body)
        .await
}

#[tokio::test]
async fn vault_validation() {
    let (client, _) = vault_client().await;
    let reply = client.arm(Method::PUT, &vault_path("1bad"), Some(json!({"location": "eastus", "properties": {}}))).await;
    assert_eq!(reply.error_code(), "VaultNameNotValid");
    let reply = client.arm(Method::PUT, &vault_path("fine-name"), Some(json!({"location": "eastus", "properties": {}}))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);

    let check = format!("{SUB}/providers/Microsoft.KeyVault/checkNameAvailability?api-version=2023-07-01");
    let reply = client.arm(Method::POST, &check, Some(json!({"name": "DEMO-vault", "type": "Microsoft.KeyVault/vaults"}))).await;
    assert_eq!(reply.json()["nameAvailable"], false);
}

#[tokio::test]
async fn the_first_request_is_challenged() {
    let (client, _) = vault_client().await;
    let reply = client.send(Method::GET, HOST, "/secrets/a?api-version=7.4", &[], "").await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let tenant = &client.config().tenant_id;
    assert_eq!(
        reply.header("www-authenticate"),
        format!("Bearer authorization=\"https://localhost:10443/{tenant}\", resource=\"https://vault.azure.localhost:10443\"")
    );

    // A management token is not a vault token.
    let wrong = format!("Bearer {}", client.token("https://management.azure.com/.default").await);
    let reply = client.send(Method::GET, HOST, "/secrets/a?api-version=7.4", &[("authorization", &wrong)], "").await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert!(reply.json()["error"]["message"].as_str().unwrap().starts_with("AKV10022"));
}

#[tokio::test]
async fn secret_lifecycle() {
    let (client, auth) = vault_client().await;

    let reply = client.send(Method::GET, HOST, "/secrets/a", &[("authorization", &auth)], "").await;
    assert_eq!(reply.error_code(), "BadParameter");
    let reply = call(&client, &auth, Method::GET, "/secrets/missing", None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.error_code(), "SecretNotFound");
    let reply = call(&client, &auth, Method::PUT, "/secrets/bad_name", Some(json!({"value": "x"}))).await;
    assert_eq!(reply.error_code(), "BadParameter");

    let reply = call(&client, &auth, Method::PUT, "/secrets/db-password", Some(json!({"value": "one", "contentType": "text/plain", "tags": {"a": "b"}}))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let first = reply.json();
    let first_id = first["id"].as_str().unwrap().to_string();
    assert!(first_id.starts_with("https://demo-vault.vault.azure.localhost:10443/secrets/db-password/"));
    assert_eq!(first["attributes"]["enabled"], true);
    assert_eq!(first["attributes"]["recoveryLevel"], "Recoverable+Purgeable");
    let first_version = first_id.rsplit('/').next().unwrap().to_string();

    let second = call(&client, &auth, Method::PUT, "/secrets/db-password", Some(json!({"value": "two"}))).await.json();
    assert_ne!(second["id"], first["id"]);

    let latest = call(&client, &auth, Method::GET, "/secrets/DB-Password", None).await.json();
    assert_eq!(latest["value"], "two");
    let old = call(&client, &auth, Method::GET, &format!("/secrets/db-password/{first_version}"), None).await.json();
    assert_eq!(old["value"], "one");
    assert_eq!(old["contentType"], "text/plain");

    let versions = call(&client, &auth, Method::GET, "/secrets/db-password/versions", None).await.json();
    assert_eq!(versions["value"].as_array().unwrap().len(), 2);
    let listing = call(&client, &auth, Method::GET, "/secrets", None).await.json();
    assert_eq!(listing["value"][0]["id"], "https://demo-vault.vault.azure.localhost:10443/secrets/db-password");
    assert!(listing["value"][0].get("value").is_none(), "listings never carry values");

    // Disabling the current version makes it unreadable.
    let reply = call(&client, &auth, Method::PATCH, "/secrets/db-password", Some(json!({"attributes": {"enabled": false}}))).await;
    assert_eq!(reply.json()["attributes"]["enabled"], false);
    let reply = call(&client, &auth, Method::GET, "/secrets/db-password", None).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);

    // Soft delete, then recover, then delete and purge.
    let deleted = call(&client, &auth, Method::DELETE, "/secrets/db-password", None).await.json();
    assert_eq!(deleted["recoveryId"], "https://demo-vault.vault.azure.localhost:10443/deletedsecrets/db-password");
    assert!(deleted["scheduledPurgeDate"].as_i64().unwrap() > deleted["deletedDate"].as_i64().unwrap());
    assert_eq!(call(&client, &auth, Method::GET, "/secrets/db-password", None).await.status, StatusCode::NOT_FOUND);
    let reply = call(&client, &auth, Method::PUT, "/secrets/db-password", Some(json!({"value": "three"}))).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(call(&client, &auth, Method::GET, "/deletedsecrets/db-password", None).await.status, StatusCode::OK);
    assert_eq!(call(&client, &auth, Method::GET, "/deletedsecrets", None).await.json()["value"].as_array().unwrap().len(), 1);

    let reply = call(&client, &auth, Method::POST, "/deletedsecrets/db-password/recover", None).await;
    assert_eq!(reply.status, StatusCode::OK);
    let versions = call(&client, &auth, Method::GET, "/secrets/db-password/versions", None).await.json();
    assert_eq!(versions["value"].as_array().unwrap().len(), 2);

    call(&client, &auth, Method::DELETE, "/secrets/db-password", None).await;
    let reply = call(&client, &auth, Method::DELETE, "/deletedsecrets/db-password", None).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert_eq!(call(&client, &auth, Method::GET, "/deletedsecrets/db-password", None).await.status, StatusCode::NOT_FOUND);
    let reply = call(&client, &auth, Method::PUT, "/secrets/db-password", Some(json!({"value": "three"}))).await;
    assert_eq!(reply.status, StatusCode::OK);

    // Deleting the vault resource removes its data plane.
    assert_eq!(client.arm(Method::DELETE, &vault_path("demo-vault"), None).await.status, StatusCode::OK);
    assert_eq!(call(&client, &auth, Method::GET, "/secrets", None).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn instances_are_isolated() {
    let (first, auth) = vault_client().await;
    call(&first, &auth, Method::PUT, "/secrets/only-here", Some(json!({"value": "x"}))).await;
    let (second, auth) = vault_client().await;
    let reply = call(&second, &auth, Method::GET, "/secrets/only-here", None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}
