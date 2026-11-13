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

fn vault_body(client: &Client, extra: Value) -> Value {
    let mut body = json!({
        "location": "eastus",
        "tags": {"team": "payments"},
        "properties": {"tenantId": client.config().tenant_id, "sku": {"family": "A", "name": "standard"}},
    });
    for (key, value) in extra.as_object().unwrap() {
        body["properties"][key] = value.clone();
    }
    body
}

#[tokio::test]
async fn vaults_are_soft_deleted_and_recoverable() {
    let (client, auth) = vault_client().await;
    call(&client, &auth, Method::PUT, "/secrets/kept", Some(json!({"value": "still here"}))).await;
    assert_eq!(client.arm(Method::DELETE, &vault_path("demo-vault"), None).await.status, StatusCode::OK);

    // The deleted vault is listed, and addressable by the location it lived in.
    let listed = client
        .arm(Method::GET, &format!("{SUB}/providers/Microsoft.KeyVault/deletedVaults?api-version=2023-07-01"), None)
        .await
        .json();
    let deleted = &listed["value"][0];
    assert_eq!(deleted["name"], "Demo-Vault");
    assert_eq!(deleted["type"], "Microsoft.KeyVault/deletedVaults");
    assert_eq!(deleted["properties"]["location"], "eastus");
    assert!(deleted["properties"]["scheduledPurgeDate"].as_str().unwrap() > deleted["properties"]["deletionDate"].as_str().unwrap());
    let one = |location: &str| {
        format!("{SUB}/providers/Microsoft.KeyVault/locations/{location}/deletedVaults/demo-vault?api-version=2023-07-01")
    };
    assert_eq!(client.arm(Method::GET, &one("eastus"), None).await.status, StatusCode::OK);
    assert_eq!(client.arm(Method::GET, &one("westus"), None).await.status, StatusCode::NOT_FOUND);

    // The name stays taken until the vault is recovered or purged.
    let check = format!("{SUB}/providers/Microsoft.KeyVault/checkNameAvailability?api-version=2023-07-01");
    let reply = client.arm(Method::POST, &check, Some(json!({"name": "demo-vault"}))).await;
    assert_eq!(reply.json()["nameAvailable"], false);
    let reply = client.arm(Method::PUT, &vault_path("demo-vault"), Some(vault_body(&client, json!({})))).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.error_code(), "ConflictError");

    // Recovery restores the settings and the contents.
    let reply = client
        .arm(Method::PUT, &vault_path("demo-vault"), Some(vault_body(&client, json!({"createMode": "recover"}))))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["properties"]["enableRbacAuthorization"], true);
    assert!(reply.json()["properties"].get("createMode").is_none());
    assert_eq!(call(&client, &auth, Method::GET, "/secrets/kept", None).await.json()["value"], "still here");
    assert_eq!(client.arm(Method::GET, &one("eastus"), None).await.status, StatusCode::NOT_FOUND);

    // Purging frees the name and drops the contents for good.
    client.arm(Method::DELETE, &vault_path("demo-vault"), None).await;
    let purge = one("eastus").replace("?", "/purge?");
    assert_eq!(client.arm(Method::POST, &purge, None).await.status, StatusCode::OK);
    assert_eq!(client.arm(Method::POST, &purge, None).await.status, StatusCode::NOT_FOUND);
    let reply = client.arm(Method::POST, &check, Some(json!({"name": "demo-vault"}))).await;
    assert_eq!(reply.json()["nameAvailable"], true);
    let reply = client.arm(Method::PUT, &vault_path("demo-vault"), Some(vault_body(&client, json!({})))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(call(&client, &auth, Method::GET, "/secrets/kept", None).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn access_policy_updates() {
    let (client, _) = vault_client().await;
    let update = |kind: &str| vault_path(&format!("demo-vault/accessPolicies/{kind}"));
    let policy = |secrets: Value| {
        json!({"properties": {"accessPolicies": [{
            "tenantId": client.config().tenant_id,
            "objectId": "aaaaaaaa-0000-0000-0000-000000000001",
            "permissions": {"secrets": secrets},
        }]}})
    };
    let stored = |reply: &Reply| reply.json()["properties"]["accessPolicies"].clone();

    let reply = client.arm(Method::PUT, &update("add"), Some(policy(json!(["get"])))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["type"], "Microsoft.KeyVault/vaults/accessPolicies");
    // Adding again grants the union; the vault resource reflects it.
    let reply = client.arm(Method::PUT, &update("add"), Some(policy(json!(["list", "GET"])))).await;
    assert_eq!(stored(&reply)[0]["permissions"]["secrets"], json!(["get", "list"]));
    let vault = client.arm(Method::GET, &vault_path("demo-vault"), None).await.json();
    assert_eq!(vault["properties"]["accessPolicies"][0]["permissions"]["secrets"], json!(["get", "list"]));

    let reply = client.arm(Method::PUT, &update("replace"), Some(policy(json!(["set"])))).await;
    assert_eq!(stored(&reply)[0]["permissions"]["secrets"], json!(["set"]));
    let reply = client.arm(Method::PUT, &update("add"), Some(policy(json!(["get"])))).await;
    let reply_remove = client.arm(Method::PUT, &update("remove"), Some(policy(json!(["set"])))).await;
    assert_eq!(stored(&reply)[0]["permissions"]["secrets"], json!(["set", "get"]));
    assert_eq!(stored(&reply_remove)[0]["permissions"]["secrets"], json!(["get"]));
    // Removing the last permission removes the policy.
    let reply = client.arm(Method::PUT, &update("remove"), Some(policy(json!(["get"])))).await;
    assert_eq!(stored(&reply), json!([]));

    assert_eq!(client.arm(Method::PUT, &update("merge"), Some(policy(json!(["get"])))).await.status, StatusCode::BAD_REQUEST);

    let links = client.arm(Method::GET, &vault_path("demo-vault/privateLinkResources"), None).await.json();
    assert_eq!(links["value"][0]["properties"]["groupId"], "vault");
    let missing = vault_path("no-such-vault/accessPolicies/add");
    assert_eq!(client.arm(Method::PUT, &missing, Some(policy(json!(["get"])))).await.error_code(), "ResourceNotFound");
}

#[tokio::test]
async fn secrets_are_one_store_seen_from_both_planes() {
    let (client, auth) = vault_client().await;
    let secret = |name: &str| vault_path(&format!("demo-vault/secrets/{name}"));

    // Created through Resource Manager, readable on the data plane.
    let reply = client
        .arm(Method::PUT, &secret("api-key"), Some(json!({"tags": {"env": "test"}, "properties": {"value": "from-arm", "contentType": "text/plain"}})))
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    let created = reply.json();
    assert_eq!(created["type"], "Microsoft.KeyVault/vaults/secrets");
    assert_eq!(created["properties"]["secretUri"], "https://demo-vault.vault.azure.localhost:10443/secrets/api-key");
    assert!(created["properties"].get("value").is_none(), "Resource Manager never returns the value");
    assert!(created["properties"]["attributes"].get("recoveryLevel").is_none());
    assert_eq!(call(&client, &auth, Method::GET, "/secrets/api-key", None).await.json()["value"], "from-arm");

    // Created on the data plane, visible through Resource Manager.
    call(&client, &auth, Method::PUT, "/secrets/db-password", Some(json!({"value": "from-data-plane"}))).await;
    let listed = client.arm(Method::GET, &secret("").replace("/secrets/?", "/secrets?"), None).await.json();
    let names: Vec<&str> = listed["value"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["api-key", "db-password"]);

    // An update without a value edits the current version; with one, it adds a version.
    let reply = client.arm(Method::PATCH, &secret("api-key"), Some(json!({"properties": {"attributes": {"enabled": false}}}))).await;
    assert_eq!(reply.json()["properties"]["attributes"]["enabled"], false);
    assert_eq!(reply.json()["properties"]["secretUriWithVersion"], created["properties"]["secretUriWithVersion"]);
    assert_eq!(call(&client, &auth, Method::GET, "/secrets/api-key", None).await.status, StatusCode::FORBIDDEN);
    let reply = client.arm(Method::PUT, &secret("api-key"), Some(json!({"properties": {"value": "rotated"}}))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_ne!(reply.json()["properties"]["secretUriWithVersion"], created["properties"]["secretUriWithVersion"]);
    let versions = call(&client, &auth, Method::GET, "/secrets/api-key/versions", None).await.json();
    assert_eq!(versions["value"].as_array().unwrap().len(), 2);

    assert_eq!(client.arm(Method::GET, &secret("missing"), None).await.error_code(), "SecretNotFound");
    assert_eq!(client.arm(Method::PUT, &secret("no-value"), Some(json!({"properties": {}}))).await.status, StatusCode::BAD_REQUEST);
    let orphan = vault_path("no-such-vault/secrets/x");
    assert_eq!(client.arm(Method::GET, &orphan, None).await.error_code(), "ResourceNotFound");
}

fn hsm_path(name: &str) -> String {
    format!("{SUB}/resourceGroups/rg/providers/Microsoft.KeyVault/managedHSMs/{name}?api-version=2023-07-01")
}

#[tokio::test]
async fn managed_hsm_lifecycle() {
    let (client, _) = vault_client().await;
    let body = |admins: Value| {
        json!({"location": "eastus", "properties": {"tenantId": client.config().tenant_id, "initialAdminObjectIds": admins}})
    };
    assert_eq!(client.arm(Method::PUT, &hsm_path("1bad"), Some(body(json!(["a"])))).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(client.arm(Method::PUT, &hsm_path("payments-hsm"), Some(body(json!([])))).await.status, StatusCode::BAD_REQUEST);

    let reply = client.arm(Method::PUT, &hsm_path("payments-hsm"), Some(body(json!(["00000000-0000-0000-0000-0000000000aa"])))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let hsm = reply.json();
    assert_eq!(hsm["sku"], json!({"family": "B", "name": "Standard_B1"}));
    assert_eq!(hsm["properties"]["hsmUri"], "https://payments-hsm.managedhsm.azure.localhost:10443/");
    assert_eq!(hsm["properties"]["securityDomainProperties"]["activationStatus"], "NotActivated");
    assert_eq!(hsm["properties"]["enableSoftDelete"], true);

    // Settings change; the tenant and administrators do not.
    let patched = client
        .arm(Method::PATCH, &hsm_path("payments-hsm"), Some(json!({"properties": {"publicNetworkAccess": "Disabled", "tenantId": "other"}})))
        .await
        .json();
    assert_eq!(patched["properties"]["publicNetworkAccess"], "Disabled");
    assert_eq!(patched["properties"]["tenantId"], client.config().tenant_id);

    let regions = client.arm(Method::GET, &hsm_path("payments-hsm/regions"), None).await.json();
    assert_eq!(regions["value"], json!([{"name": "eastus", "isPrimary": true, "provisioningState": "Succeeded"}]));
    let links = client.arm(Method::GET, &hsm_path("payments-hsm/privateLinkResources"), None).await.json();
    assert_eq!(links["value"][0]["properties"]["groupId"], "managedhsm");

    let check = format!("{SUB}/providers/Microsoft.KeyVault/checkMhsmNameAvailability?api-version=2023-07-01");
    assert_eq!(client.arm(Method::POST, &check, Some(json!({"name": "payments-hsm"}))).await.json()["nameAvailable"], false);

    // Soft delete keeps the name until a purge; recovery restores the settings.
    assert_eq!(client.arm(Method::DELETE, &hsm_path("payments-hsm"), None).await.status, StatusCode::OK);
    let deleted = format!("{SUB}/providers/Microsoft.KeyVault/locations/eastus/deletedManagedHSMs/payments-hsm?api-version=2023-07-01");
    let reply = client.arm(Method::GET, &deleted, None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert!(reply.json()["properties"]["mhsmId"].as_str().unwrap().ends_with("/managedHSMs/payments-hsm"));
    let listed = client.arm(Method::GET, &format!("{SUB}/providers/Microsoft.KeyVault/deletedManagedHSMs?api-version=2023-07-01"), None).await.json();
    assert_eq!(listed["value"].as_array().unwrap().len(), 1);
    assert_eq!(client.arm(Method::POST, &check, Some(json!({"name": "payments-hsm"}))).await.json()["nameAvailable"], false);
    let reply = client.arm(Method::PUT, &hsm_path("payments-hsm"), Some(body(json!(["x"])))).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);

    let mut recover = body(json!(["x"]));
    recover["properties"]["createMode"] = json!("recover");
    let reply = client.arm(Method::PUT, &hsm_path("payments-hsm"), Some(recover)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["properties"]["publicNetworkAccess"], "Disabled");
    assert_eq!(client.arm(Method::GET, &deleted, None).await.status, StatusCode::NOT_FOUND);

    client.arm(Method::DELETE, &hsm_path("payments-hsm"), None).await;
    let purge = deleted.replace("?", "/purge?");
    let reply = client.arm(Method::POST, &purge, None).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    let status_url = reply.header("location").strip_prefix("https://localhost:10443").expect("a status URL on the emulator").to_string();
    assert_eq!(client.arm(Method::GET, &status_url, None).await.status, StatusCode::OK);
    assert_eq!(client.arm(Method::POST, &check, Some(json!({"name": "payments-hsm"}))).await.json()["nameAvailable"], true);
}

#[tokio::test]
async fn keys_through_resource_manager() {
    let (client, _) = vault_client().await;
    let key = |rest: &str| vault_path(&format!("demo-vault/keys{rest}"));

    let reply = client.arm(Method::PUT, &key("/signing"), Some(json!({"properties": {"kty": "EC"}}))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let created = reply.json();
    assert_eq!(created["type"], "Microsoft.KeyVault/vaults/keys");
    assert_eq!(created["location"], "eastus");
    assert_eq!(created["properties"]["curveName"], "P-256");
    assert_eq!(created["properties"]["keyOps"], json!(["sign", "verify"]));
    assert_eq!(created["properties"]["keyUri"], "https://demo-vault.vault.azure.localhost:10443/keys/signing");
    assert_eq!(created["properties"]["attributes"]["enabled"], true);

    // Creating it again returns the same key: no new version, settings ignored.
    let again = client.arm(Method::PUT, &key("/signing"), Some(json!({"properties": {"kty": "RSA"}}))).await.json();
    assert_eq!(again["properties"]["keyUriWithVersion"], created["properties"]["keyUriWithVersion"]);
    let versions = client.arm(Method::GET, &key("/signing/versions"), None).await.json();
    assert_eq!(versions["value"].as_array().unwrap().len(), 1);
    let version = created["properties"]["keyUriWithVersion"].as_str().unwrap().rsplit('/').next().unwrap().to_string();
    let one = client.arm(Method::GET, &key(&format!("/signing/versions/{version}")), None).await.json();
    assert_eq!(one["type"], "Microsoft.KeyVault/vaults/keys/versions");
    assert_eq!(one["name"], version.as_str());

    let rsa = client
        .arm(Method::PUT, &key("/wrapping"), Some(json!({"tags": {"use": "wrap"}, "properties": {"kty": "RSA", "keySize": 4096, "keyOps": ["wrapKey", "unwrapKey"]}})))
        .await
        .json();
    assert_eq!(rsa["properties"]["keySize"], 4096);
    assert_eq!(rsa["tags"]["use"], "wrap");
    let listed = client.arm(Method::GET, &key(""), None).await.json();
    let names: Vec<&str> = listed["value"].as_array().unwrap().iter().map(|k| k["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["signing", "wrapping"]);

    for (body, why) in [
        (json!({"properties": {}}), "kty is required"),
        (json!({"properties": {"kty": "RSA", "keySize": 1024}}), "RSA sizes"),
        (json!({"properties": {"kty": "EC", "curveName": "P-999"}}), "curves"),
        (json!({"properties": {"kty": "EC", "keyOps": ["encrypt"]}}), "EC keys cannot encrypt"),
        (json!({"properties": {"kty": "RSA-HSM"}}), "a standard vault holds no HSM keys"),
        (json!({"properties": {"kty": "oct-HSM"}}), "vaults hold no symmetric keys"),
    ] {
        let reply = client.arm(Method::PUT, &key("/rejected"), Some(body)).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{why}: {}", reply.text());
    }
    assert_eq!(client.arm(Method::GET, &key("/missing"), None).await.error_code(), "KeyNotFound");

    // Keys survive a vault's soft delete and recovery.
    client.arm(Method::DELETE, &vault_path("demo-vault"), None).await;
    let mut recover = vault_body(&client, json!({"createMode": "recover"}));
    recover["properties"]["enableRbacAuthorization"] = json!(true);
    assert_eq!(client.arm(Method::PUT, &vault_path("demo-vault"), Some(recover)).await.status, StatusCode::OK);
    assert_eq!(client.arm(Method::GET, &key("/signing"), None).await.status, StatusCode::OK);
}

#[tokio::test]
async fn managed_hsm_keys() {
    let (client, _) = vault_client().await;
    let body = json!({"location": "eastus", "properties": {"tenantId": client.config().tenant_id, "initialAdminObjectIds": ["a"]}});
    client.arm(Method::PUT, &hsm_path("keys-hsm"), Some(body)).await;
    let key = |rest: &str| hsm_path(&format!("keys-hsm/keys{rest}"));

    let reply = client.arm(Method::PUT, &key("/master"), Some(json!({"properties": {"kty": "oct-HSM"}}))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let created = reply.json();
    assert_eq!(created["type"], "Microsoft.KeyVault/managedHSMs/keys");
    assert_eq!(created["properties"]["keySize"], 256);
    assert!(created.get("location").is_none());
    assert_eq!(created["properties"]["keyUri"], "https://keys-hsm.managedhsm.azure.localhost:10443/keys/master");
    assert_eq!(client.arm(Method::GET, &key(""), None).await.json()["value"].as_array().unwrap().len(), 1);

    // Purging the HSM destroys its keys.
    client.arm(Method::DELETE, &hsm_path("keys-hsm"), None).await;
    let purge = format!("{SUB}/providers/Microsoft.KeyVault/locations/eastus/deletedManagedHSMs/keys-hsm/purge?api-version=2023-07-01");
    assert_eq!(client.arm(Method::POST, &purge, None).await.status, StatusCode::ACCEPTED);
    let body = json!({"location": "eastus", "properties": {"tenantId": client.config().tenant_id, "initialAdminObjectIds": ["a"]}});
    client.arm(Method::PUT, &hsm_path("keys-hsm"), Some(body)).await;
    assert_eq!(client.arm(Method::GET, &key(""), None).await.json(), json!({"value": []}));
}
