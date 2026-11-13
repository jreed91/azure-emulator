use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use http::{Method, StatusCode};
use serde_json::{json, Value};
use sha2::Sha256;

use crate::support::{Client, STORAGE_SCOPE, SUB};

const API: &str = "api-version=2023-01-01";

fn account_path(group: &str, name: &str) -> String {
    format!("{SUB}/resourceGroups/{group}/providers/Microsoft.Storage/storageAccounts/{name}")
}

async fn create_account(client: &Client, group: &str, name: &str) {
    let body = json!({"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}});
    let reply = client.arm(Method::PUT, &format!("{}?{API}", account_path(group, name)), Some(body)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
}

#[tokio::test]
async fn storage_account_lifecycle() {
    let client = Client::new();
    let path = format!("{}?{API}", account_path("rg", "demoaccount"));
    let body = json!({"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}, "tags": {"t": "1"}});

    let reply = client.arm(Method::PUT, &path, Some(body.clone())).await;
    assert_eq!(reply.error_code(), "ResourceGroupNotFound");
    client.create_group("rg").await;
    client.create_group("other").await;

    let reply = client
        .arm(Method::PUT, &format!("{}?{API}", account_path("rg", "Bad_Name")), Some(body.clone()))
        .await;
    assert_eq!(reply.error_code(), "AccountNameInvalid");

    let reply = client.arm(Method::PUT, &path, Some(body.clone())).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let account = reply.json();
    assert_eq!(account["id"], account_path("rg", "demoaccount"));
    assert_eq!(account["type"], "Microsoft.Storage/storageAccounts");
    assert_eq!(account["kind"], "StorageV2");
    assert_eq!(account["properties"]["provisioningState"], "Succeeded");
    assert_eq!(account["properties"]["accessTier"], "Hot");
    assert_eq!(
        account["properties"]["primaryEndpoints"]["blob"],
        "https://demoaccount.blob.core.azure.localhost:10443/"
    );

    // Account names are global, across resource groups.
    let reply = client
        .arm(Method::PUT, &format!("{}?{API}", account_path("other", "demoaccount")), Some(body.clone()))
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.error_code(), "StorageAccountAlreadyTaken");

    let check = format!("{SUB}/providers/Microsoft.Storage/checkNameAvailability?{API}");
    let reply = client.arm(Method::POST, &check, Some(json!({"name": "demoaccount", "type": "Microsoft.Storage/storageAccounts"}))).await;
    assert_eq!(reply.json()["nameAvailable"], false);
    assert_eq!(reply.json()["reason"], "AlreadyExists");

    let reply = client.arm(Method::PATCH, &path, Some(json!({"tags": {"t": "2"}, "properties": {"minimumTlsVersion": "TLS1_2"}}))).await;
    assert_eq!(reply.json()["tags"]["t"], "2");
    assert_eq!(reply.json()["properties"]["minimumTlsVersion"], "TLS1_2");

    let keys_path = format!("{}/listKeys?{API}", account_path("rg", "demoaccount"));
    let keys = client.arm(Method::POST, &keys_path, None).await.json();
    let key1 = keys["keys"][0]["value"].as_str().unwrap().to_string();
    assert_eq!(STANDARD.decode(&key1).unwrap().len(), 64);
    let regenerate = format!("{}/regenerateKey?{API}", account_path("rg", "demoaccount"));
    let regenerated = client.arm(Method::POST, &regenerate, Some(json!({"keyName": "key1"}))).await.json();
    assert_ne!(regenerated["keys"][0]["value"], key1);
    assert_eq!(regenerated["keys"][1]["value"], keys["keys"][1]["value"]);

    for list in [
        format!("{SUB}/providers/Microsoft.Storage/storageAccounts?{API}"),
        format!("{SUB}/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts?{API}"),
        format!("{SUB}/resourceGroups/rg/resources?api-version=2021-04-01"),
    ] {
        let reply = client.arm(Method::GET, &list, None).await;
        assert_eq!(reply.json()["value"].as_array().unwrap().len(), 1, "{list}");
    }

    // Deleting the group deletes the account and frees its name.
    let reply = client.arm(Method::DELETE, &format!("{SUB}/resourcegroups/rg?api-version=2021-04-01"), None).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    let reply = client.arm(Method::POST, &check, Some(json!({"name": "demoaccount"}))).await;
    assert_eq!(reply.json()["nameAvailable"], true);
}

#[tokio::test]
async fn blob_round_trip_with_a_bearer_token() {
    let client = Client::new();
    client.create_group("rg").await;
    create_account(&client, "rg", "blobacct").await;
    let host = "blobacct.blob.core.azure.localhost:10443";
    let authorization = format!("Bearer {}", client.token(STORAGE_SCOPE).await);
    let auth = [("authorization", authorization.as_str()), ("x-ms-version", "2023-11-03")];

    let reply = client.send(Method::GET, host, "/?comp=list", &[], "").await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.header("x-ms-error-code"), "NoAuthenticationInformation");

    let reply = client.send(Method::PUT, host, "/Bad_Name?restype=container", &auth, "").await;
    assert_eq!(reply.header("x-ms-error-code"), "InvalidResourceName");
    let reply = client.send(Method::PUT, host, "/photos?restype=container", &auth, "").await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.header("x-ms-version"), "2023-11-03");
    let reply = client.send(Method::PUT, host, "/photos?restype=container", &auth, "").await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.header("x-ms-error-code"), "ContainerAlreadyExists");
    assert!(reply.text().contains("<Code>ContainerAlreadyExists</Code>"));

    let reply = client.send(Method::PUT, host, "/photos/a.txt", &auth, "hello").await;
    assert_eq!(reply.header("x-ms-error-code"), "MissingRequiredHeader");
    let mut put = auth.to_vec();
    put.extend([("x-ms-blob-type", "BlockBlob"), ("x-ms-blob-content-type", "text/plain"), ("x-ms-meta-owner", "me")]);
    let reply = client.send(Method::PUT, host, "/photos/2024/a%20b.txt", &put, "hello world").await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    assert_eq!(reply.header("content-md5"), "XrY7u+Ae7tCTyyK7j1rNww==");
    for blob in ["/photos/2024/c.txt", "/photos/2025/d.txt", "/photos/top.txt"] {
        assert_eq!(client.send(Method::PUT, host, blob, &put, "x").await.status, StatusCode::CREATED);
    }

    let mut conditional = put.clone();
    conditional.push(("if-none-match", "*"));
    let reply = client.send(Method::PUT, host, "/photos/top.txt", &conditional, "y").await;
    assert_eq!(reply.header("x-ms-error-code"), "BlobAlreadyExists");

    let reply = client.send(Method::GET, host, "/photos/2024/a%20b.txt", &auth, "").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text(), "hello world");
    assert_eq!(reply.header("content-type"), "text/plain");
    assert_eq!(reply.header("x-ms-meta-owner"), "me");
    assert_eq!(reply.header("x-ms-blob-type"), "BlockBlob");

    let mut ranged = auth.to_vec();
    ranged.push(("x-ms-range", "bytes=6-1000"));
    let reply = client.send(Method::GET, host, "/photos/2024/a%20b.txt", &ranged, "").await;
    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(reply.text(), "world");
    assert_eq!(reply.header("content-range"), "bytes 6-10/11");
    ranged[2] = ("x-ms-range", "bytes=11-");
    let reply = client.send(Method::GET, host, "/photos/2024/a%20b.txt", &ranged, "").await;
    assert_eq!(reply.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(reply.header("x-ms-error-code"), "InvalidRange");

    let reply = client.send(Method::HEAD, host, "/photos/2024/a%20b.txt", &auth, "").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-length"), "11");
    assert!(reply.body.is_empty());

    let reply = client
        .send(Method::GET, host, "/photos?restype=container&comp=list&delimiter=%2F", &auth, "")
        .await;
    let listing = reply.text();
    assert!(listing.contains("<BlobPrefix><Name>2024/</Name></BlobPrefix>"), "{listing}");
    assert!(listing.contains("<BlobPrefix><Name>2025/</Name></BlobPrefix>"));
    assert!(listing.contains("<Name>top.txt</Name>"));
    assert!(!listing.contains("a b.txt"));

    // Paging: two per page over four blobs, then the rest from the marker.
    let reply = client.send(Method::GET, host, "/photos?restype=container&comp=list&maxresults=2", &auth, "").await;
    let page = reply.text();
    assert!(page.contains("<NextMarker>2024/c.txt</NextMarker>"), "{page}");
    let reply = client
        .send(Method::GET, host, "/photos?restype=container&comp=list&maxresults=2&marker=2024%2Fc.txt", &auth, "")
        .await;
    let page = reply.text();
    assert!(page.contains("<Name>2025/d.txt</Name>") && page.contains("<Name>top.txt</Name>"));
    assert!(page.contains("<NextMarker />"));

    let reply = client.send(Method::GET, host, "/?comp=list", &auth, "").await;
    assert!(reply.text().contains("<Container><Name>photos</Name>"));

    let reply = client.send(Method::DELETE, host, "/photos/top.txt", &auth, "").await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    let reply = client.send(Method::GET, host, "/photos/top.txt", &auth, "").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.header("x-ms-error-code"), "BlobNotFound");

    let reply = client.send(Method::DELETE, host, "/photos?restype=container", &auth, "").await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    let reply = client.send(Method::GET, host, "/photos?restype=container", &auth, "").await;
    assert_eq!(reply.header("x-ms-error-code"), "ContainerNotFound");

    // Unimplemented operations say so.
    let reply = client.send(Method::GET, host, "/?restype=service&comp=properties", &auth, "").await;
    assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED);
}

/// Signs a body-less request the way a storage SDK does.
fn sign(key: &str, method: &str, account: &str, path: &str, canonical_query: &str, date: &str) -> String {
    let to_sign = format!(
        "{method}\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:{date}\nx-ms-version:2023-11-03\n/{account}{path}{canonical_query}"
    );
    let mut mac = Hmac::<Sha256>::new_from_slice(&STANDARD.decode(key).unwrap()).unwrap();
    mac.update(to_sign.as_bytes());
    format!("SharedKey {account}:{}", STANDARD.encode(mac.finalize().into_bytes()))
}

#[tokio::test]
async fn shared_key_signatures_are_verified() {
    let client = Client::new();
    client.create_group("rg").await;
    create_account(&client, "rg", "keyacct").await;
    let keys_path = format!("{}/listKeys?{API}", account_path("rg", "keyacct"));
    let keys = client.arm(Method::POST, &keys_path, None).await.json();
    let key2 = keys["keys"][1]["value"].as_str().unwrap();
    let host = "keyacct.blob.core.azure.localhost:10443";
    let date = httpdate::fmt_http_date(std::time::SystemTime::now());

    let authorization = sign(key2, "PUT", "keyacct", "/docs", "\nrestype:container", &date);
    let headers = [("authorization", authorization.as_str()), ("x-ms-date", date.as_str()), ("x-ms-version", "2023-11-03")];
    let reply = client.send(Method::PUT, host, "/docs?restype=container", &headers, "").await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());

    // The same signature does not authorize a different request.
    let reply = client.send(Method::PUT, host, "/other?restype=container", &headers, "").await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.header("x-ms-error-code"), "AuthenticationFailed");

    let authorization = sign(key2, "GET", "keyacct", "/", "\ncomp:list", &date);
    let headers = [("authorization", authorization.as_str()), ("x-ms-date", date.as_str()), ("x-ms-version", "2023-11-03")];
    let reply = client.send(Method::GET, host, "/?comp=list", &headers, "").await;
    assert!(reply.text().contains("<Name>docs</Name>"), "{}", reply.text());

    // A request for an account that does not exist.
    let reply = client.send(Method::GET, "ghost.blob.core.azure.localhost:10443", "/?comp=list", &headers, "").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

fn child(rest: &str) -> String {
    format!("{}/{rest}?{API}", account_path("rg", "mgmtacct"))
}

async fn management_client() -> Client {
    let client = Client::new();
    client.create_group("rg").await;
    create_account(&client, "rg", "mgmtacct").await;
    client
}

#[tokio::test]
async fn service_singletons_come_with_the_account() {
    let client = management_client().await;
    for service in ["blobServices", "fileServices", "queueServices", "tableServices"] {
        let reply = client.arm(Method::GET, &child(&format!("{service}/default")), None).await;
        assert_eq!(reply.status, StatusCode::OK, "{service}: {}", reply.text());
        let body = reply.json();
        assert_eq!(body["name"], "default");
        assert_eq!(body["sku"]["name"], "Standard_LRS", "a service reports its account's SKU");
        assert_eq!(body["properties"]["cors"]["corsRules"], json!([]));
        let listed = client.arm(Method::GET, &child(service), None).await.json();
        assert_eq!(listed["value"].as_array().unwrap().len(), 1);
    }

    // Setting properties changes what is named and keeps the rest.
    let update = json!({"properties": {"deleteRetentionPolicy": {"enabled": true, "days": 14}, "isVersioningEnabled": true}});
    let reply = client.arm(Method::PUT, &child("blobServices/default"), Some(update)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let stored = client.arm(Method::GET, &child("blobServices/default"), None).await.json();
    assert_eq!(stored["properties"]["deleteRetentionPolicy"]["days"], 14);
    assert_eq!(stored["properties"]["isVersioningEnabled"], true);
    assert_eq!(stored["properties"]["cors"]["corsRules"], json!([]));

    // There is no other service to create.
    let reply = client.arm(Method::PUT, &child("blobServices/other"), Some(json!({"properties": {}}))).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn containers_are_one_store_seen_from_both_planes() {
    let client = management_client().await;
    let host = "mgmtacct.blob.core.azure.localhost:10443";
    let authorization = format!("Bearer {}", client.token(STORAGE_SCOPE).await);
    let auth = [("authorization", authorization.as_str())];
    let container = |name: &str| child(&format!("blobServices/default/containers/{name}"));

    assert_eq!(client.arm(Method::PUT, &container("Bad_Name"), Some(json!({}))).await.status, StatusCode::BAD_REQUEST);
    let reply = client
        .arm(Method::PUT, &container("from-arm"), Some(json!({"properties": {"metadata": {"owner": "ops"}, "publicAccess": "Blob"}})))
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    let created = reply.json();
    assert_eq!(created["type"], "Microsoft.Storage/storageAccounts/blobServices/containers");
    assert_eq!(created["properties"]["publicAccess"], "Blob");
    assert_eq!(created["properties"]["leaseState"], "Available");
    assert_eq!(created["properties"]["defaultEncryptionScope"], "$account-encryption-key");

    // Created through Resource Manager, usable on the data plane, metadata included.
    let reply = client.send(Method::GET, host, "/from-arm?restype=container", &auth, "").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("x-ms-meta-owner"), "ops");
    let put = [auth[0], ("x-ms-blob-type", "BlockBlob")];
    assert_eq!(client.send(Method::PUT, host, "/from-arm/a.txt", &put, "x").await.status, StatusCode::CREATED);

    // Created on the data plane, visible through Resource Manager.
    client.send(Method::PUT, host, "/from-data-plane?restype=container", &auth, "").await;
    let listed = client.arm(Method::GET, &container("").replace("/containers/?", "/containers?"), None).await.json();
    let names: Vec<&str> = listed["value"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["from-arm", "from-data-plane"]);

    // An update changes the entity tag and keeps the blobs; a refused one changes nothing.
    let reply = client.arm(Method::PATCH, &container("from-arm"), Some(json!({"properties": {"publicAccess": "None"}}))).await;
    assert_eq!(reply.json()["properties"]["publicAccess"], "None");
    assert_ne!(reply.json()["etag"], created["etag"]);
    let reply = client.arm(Method::PATCH, &container("from-arm"), Some(json!({"properties": {"publicAccess": "World"}}))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(client.send(Method::GET, host, "/from-arm/a.txt", &auth, "").await.text(), "x");
    assert_eq!(client.arm(Method::PATCH, &container("missing"), Some(json!({}))).await.error_code(), "ContainerNotFound");

    assert_eq!(client.arm(Method::DELETE, &container("from-arm"), None).await.status, StatusCode::OK);
    assert_eq!(client.arm(Method::DELETE, &container("from-arm"), None).await.status, StatusCode::NO_CONTENT);
    let reply = client.send(Method::GET, host, "/from-arm?restype=container", &auth, "").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let orphan = format!("{}/blobServices/default/containers/x?{API}", account_path("rg", "ghostacct"));
    assert_eq!(client.arm(Method::GET, &orphan, None).await.error_code(), "ResourceNotFound");
}

#[tokio::test]
async fn queues_tables_and_shares() {
    let client = management_client().await;

    let reply = client.arm(Method::PUT, &child("queueServices/default/queues/jobs"), Some(json!({"properties": {"metadata": {"a": "b"}}}))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["properties"]["approximateMessageCount"], 0);
    assert_eq!(client.arm(Method::PUT, &child("queueServices/default/queues/Bad_Queue"), Some(json!({}))).await.status, StatusCode::BAD_REQUEST);

    // A table is created with no request body at all.
    let reply = client.arm(Method::PUT, &child("tableServices/default/tables/Customers"), None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["properties"]["tableName"], "Customers");
    assert_eq!(client.arm(Method::PUT, &child("tableServices/default/tables/1bad"), Some(json!({}))).await.status, StatusCode::BAD_REQUEST);

    let share = child("fileServices/default/shares/docs");
    let reply = client.arm(Method::PUT, &share, Some(json!({"properties": {"shareQuota": 100}}))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["properties"]["shareQuota"], 100);
    assert_eq!(reply.json()["properties"]["enabledProtocols"], "SMB");
    let reply = client.arm(Method::PATCH, &share, Some(json!({"properties": {"shareQuota": 200}}))).await;
    assert_eq!(reply.json()["properties"]["shareQuota"], 200);
    assert_eq!(client.arm(Method::PUT, &share, Some(json!({"properties": {"shareQuota": 0}}))).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(client.arm(Method::PUT, &share, Some(json!({"properties": {"enabledProtocols": "NFS"}}))).await.status, StatusCode::BAD_REQUEST);

    // An update cannot set what the service owns.
    let reply = client
        .arm(Method::PATCH, &child("queueServices/default/queues/jobs"), Some(json!({"properties": {"approximateMessageCount": 99, "metadata": {"c": "d"}}})))
        .await;
    assert_eq!(reply.json()["properties"]["approximateMessageCount"], 0);
    assert_eq!(reply.json()["properties"]["metadata"], json!({"c": "d"}));

    for (collection, count) in [("queueServices/default/queues", 1), ("tableServices/default/tables", 1), ("fileServices/default/shares", 1)] {
        let listed = client.arm(Method::GET, &child(collection), None).await.json();
        assert_eq!(listed["value"].as_array().unwrap().len(), count, "{collection}");
    }
    // The resource group's generic listing shows the account, not its children.
    let listed = client.arm(Method::GET, &format!("{SUB}/resourceGroups/rg/resources?api-version=2021-04-01"), None).await.json();
    assert_eq!(listed["value"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn encryption_scopes_and_policies() {
    let client = management_client().await;

    let scope = child("encryptionScopes/tenant-a");
    let reply = client.arm(Method::PUT, &scope, Some(json!({"properties": {"requireInfrastructureEncryption": true}}))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["properties"]["source"], "Microsoft.Storage");
    assert_eq!(reply.json()["properties"]["state"], "Enabled");
    let reply = client.arm(Method::PATCH, &scope, Some(json!({"properties": {"state": "Disabled", "requireInfrastructureEncryption": false}}))).await;
    assert_eq!(reply.json()["properties"]["state"], "Disabled");
    assert_eq!(reply.json()["properties"]["requireInfrastructureEncryption"], true, "fixed at creation");
    let reply = client.arm(Method::PATCH, &scope, Some(json!({"properties": {"state": "Paused"}}))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = client.arm(Method::PUT, &child("encryptionScopes/byok"), Some(json!({"properties": {"source": "Microsoft.KeyVault"}}))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "a customer-managed scope needs a key");

    let rules = json!({"rules": [{"name": "expire", "enabled": true, "type": "Lifecycle", "definition": {"actions": {"baseBlob": {"delete": {"daysAfterModificationGreaterThan": 30}}}, "filters": {"blobTypes": ["blockBlob"]}}}]});
    let policy = child("managementPolicies/default");
    assert_eq!(client.arm(Method::GET, &policy, None).await.status, StatusCode::NOT_FOUND);
    let reply = client.arm(Method::PUT, &policy, Some(json!({"properties": {"policy": rules}}))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["properties"]["policy"]["rules"][0]["name"], "expire");
    assert!(reply.json()["properties"]["lastModifiedTime"].is_string());
    assert_eq!(client.arm(Method::PUT, &child("managementPolicies/other"), Some(json!({"properties": {"policy": rules}}))).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(client.arm(Method::PUT, &policy, Some(json!({"properties": {}}))).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(client.arm(Method::DELETE, &policy, None).await.status, StatusCode::OK);

    // A locally redundant account has one private link target per service.
    let links = client.arm(Method::GET, &child("privateLinkResources"), None).await.json();
    let groups: Vec<&str> = links["value"].as_array().unwrap().iter().map(|l| l["name"].as_str().unwrap()).collect();
    assert_eq!(groups, ["blob", "table", "queue", "file", "web", "dfs"]);

    let inventory = child("inventoryPolicies/default");
    let body = json!({"properties": {"policy": {"enabled": true, "type": "Inventory", "rules": []}}});
    assert_eq!(client.arm(Method::PUT, &inventory, Some(body)).await.status, StatusCode::OK);
    assert_eq!(client.arm(Method::GET, &child("inventoryPolicies"), None).await.json()["value"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn shared_access_signatures_issued_by_resource_manager() {
    let client = management_client().await;
    let host = "mgmtacct.blob.core.azure.localhost:10443";
    let bearer = format!("Bearer {}", client.token(STORAGE_SCOPE).await);
    let put = [("authorization", bearer.as_str()), ("x-ms-blob-type", "BlockBlob")];
    client.send(Method::PUT, host, "/reports?restype=container", &put[..1], "").await;
    client.send(Method::PUT, host, "/reports/q1.csv", &put, "a,b").await;
    client.send(Method::PUT, host, "/other?restype=container", &put[..1], "").await;
    let action = |name: &str| format!("{}/{name}?{API}", account_path("rg", "mgmtacct"));
    let client = &client;
    let anonymous = |path: String| async move { client.send(Method::GET, host, &path, &[], "").await };

    // An account SAS: read and list on containers and objects, not the service.
    let request = json!({"signedServices": "b", "signedResourceTypes": "co", "signedPermission": "rl", "signedExpiryTime": "2099-01-01T00:00:00Z", "keyToSign": "key2"});
    let reply = client.arm(Method::POST, &action("listAccountSas"), Some(request)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let token = reply.json()["accountSasToken"].as_str().unwrap().to_string();
    assert!(token.starts_with("sv=2015-04-05&ss=b&srt=co&sp=rl&se=2099-01-01T00%3A00%3A00Z&sig="), "{token}");

    let reply = anonymous(format!("/reports/q1.csv?{token}")).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.text(), "a,b");
    assert_eq!(anonymous(format!("/reports?restype=container&comp=list&{token}")).await.status, StatusCode::OK);
    let reply = anonymous(format!("/?comp=list&{token}")).await;
    assert_eq!(reply.header("x-ms-error-code"), "AuthorizationResourceTypeMismatch");
    let reply = client.send(Method::DELETE, host, &format!("/reports/q1.csv?{token}"), &[], "").await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.header("x-ms-error-code"), "AuthorizationPermissionMismatch");

    // Changing anything the signature covers invalidates the token.
    let widened = token.replace("sp=rl", "sp=rld");
    let reply = anonymous(format!("/reports/q1.csv?{widened}")).await;
    assert_eq!(reply.header("x-ms-error-code"), "AuthenticationFailed");

    // A service SAS is bound to its container.
    let request = json!({"canonicalizedResource": "/blob/mgmtacct/reports", "signedResource": "c", "signedPermission": "rl", "signedExpiryTime": "2099-01-01"});
    let reply = client.arm(Method::POST, &action("listServiceSas"), Some(request)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let token = reply.json()["serviceSasToken"].as_str().unwrap().to_string();
    assert_eq!(anonymous(format!("/reports/q1.csv?{token}")).await.text(), "a,b");
    assert!(anonymous(format!("/reports?restype=container&comp=list&{token}")).await.text().contains("q1.csv"));
    assert_eq!(anonymous(format!("/other?restype=container&comp=list&{token}")).await.header("x-ms-error-code"), "AuthenticationFailed");
    // ... and cannot manage the container itself.
    let reply = client.send(Method::DELETE, host, &format!("/reports?restype=container&{token}"), &[], "").await;
    assert_eq!(reply.header("x-ms-error-code"), "AuthorizationPermissionMismatch");

    // Expired tokens are issued as asked and then refused.
    let request = json!({"signedServices": "b", "signedResourceTypes": "o", "signedPermission": "r", "signedExpiryTime": "2001-01-01T00:00:00Z"});
    let token = client.arm(Method::POST, &action("listAccountSas"), Some(request)).await.json()["accountSasToken"].as_str().unwrap().to_string();
    assert_eq!(anonymous(format!("/reports/q1.csv?{token}")).await.header("x-ms-error-code"), "AuthenticationFailed");

    // Regenerating the signing key revokes what it signed.
    let request = json!({"signedServices": "b", "signedResourceTypes": "o", "signedPermission": "r", "signedExpiryTime": "2099-01-01"});
    let token = client.arm(Method::POST, &action("listAccountSas"), Some(request)).await.json()["accountSasToken"].as_str().unwrap().to_string();
    assert_eq!(anonymous(format!("/reports/q1.csv?{token}")).await.status, StatusCode::OK);
    client.arm(Method::POST, &action("regenerateKey"), Some(json!({"keyName": "key1"}))).await;
    assert_eq!(anonymous(format!("/reports/q1.csv?{token}")).await.status, StatusCode::FORBIDDEN);

    for bad in [json!({"signedServices": "b"}), json!({"signedServices": "b", "signedResourceTypes": "o", "signedPermission": "r", "signedExpiryTime": "soon"})] {
        assert_eq!(client.arm(Method::POST, &action("listAccountSas"), Some(bad)).await.status, StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn local_users() {
    let client = management_client().await;
    let user = child("localUsers/sftpuser");
    assert_eq!(client.arm(Method::PUT, &child("localUsers/Bad-Name"), Some(json!({}))).await.status, StatusCode::BAD_REQUEST);

    let body = json!({"properties": {
        "homeDirectory": "uploads/",
        "hasSharedKey": true,
        "permissionScopes": [{"permissions": "rw", "service": "blob", "resourceName": "uploads"}],
        "sshAuthorizedKeys": [{"description": "laptop", "key": "ssh-ed25519 AAAA"}],
        "sid": "forged",
    }});
    let reply = client.arm(Method::PUT, &user, Some(body)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let created = reply.json();
    assert!(created["properties"]["sid"].as_str().unwrap().starts_with("S-1-2-0-"));
    assert_eq!(created["properties"]["hasSshKey"], true);
    assert_eq!(created["properties"]["hasSshPassword"], false);
    assert!(created["properties"].get("sshAuthorizedKeys").is_none(), "keys are only returned by listKeys");

    let keys = client.arm(Method::POST, &child("localUsers/sftpuser/listKeys"), None).await.json();
    assert_eq!(keys["sshAuthorizedKeys"][0]["description"], "laptop");
    let shared = keys["sharedKey"].as_str().unwrap().to_string();
    assert_eq!(client.arm(Method::POST, &child("localUsers/sftpuser/listKeys"), None).await.json()["sharedKey"], shared.as_str());

    let first = client.arm(Method::POST, &child("localUsers/sftpuser/regeneratePassword"), None).await.json();
    let second = client.arm(Method::POST, &child("localUsers/sftpuser/regeneratePassword"), None).await.json();
    assert_eq!(first["sshPassword"].as_str().unwrap().len(), 32);
    assert_ne!(first["sshPassword"], second["sshPassword"]);
    let stored = client.arm(Method::GET, &user, None).await.json();
    assert_eq!(stored["properties"]["hasSshPassword"], true);
    assert_eq!(stored["properties"]["sid"], created["properties"]["sid"]);
}

#[tokio::test]
async fn legal_holds_and_immutability_policies_are_enforced() {
    let client = management_client().await;
    let host = "mgmtacct.blob.core.azure.localhost:10443";
    let bearer = format!("Bearer {}", client.token(STORAGE_SCOPE).await);
    let auth = [("authorization", bearer.as_str())];
    let put = [auth[0], ("x-ms-blob-type", "BlockBlob")];
    let container = |rest: &str| child(&format!("blobServices/default/containers/records{rest}"));
    client.arm(Method::PUT, &container(""), Some(json!({}))).await;
    client.send(Method::PUT, host, "/records/a.txt", &put, "v1").await;

    // A legal hold freezes existing blobs but allows new ones.
    let reply = client.arm(Method::POST, &container("/setLegalHold"), Some(json!({"tags": ["Case123", "audit2026"]}))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json(), json!({"hasLegalHold": true, "tags": ["case123", "audit2026"]}));
    assert_eq!(client.arm(Method::POST, &container("/setLegalHold"), Some(json!({"tags": ["x"]}))).await.status, StatusCode::BAD_REQUEST);
    let reply = client.send(Method::PUT, host, "/records/a.txt", &put, "v2").await;
    assert_eq!(reply.header("x-ms-error-code"), "BlobImmutableDueToLegalHold");
    let reply = client.send(Method::DELETE, host, "/records/a.txt", &auth, "").await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(client.send(Method::PUT, host, "/records/b.txt", &put, "new").await.status, StatusCode::CREATED);
    let reply = client.send(Method::DELETE, host, "/records?restype=container", &auth, "").await;
    assert_eq!(reply.header("x-ms-error-code"), "ContainerProtectedFromDeletion");
    assert_eq!(client.arm(Method::DELETE, &container(""), None).await.status, StatusCode::CONFLICT);
    let reply = client.send(Method::GET, host, "/records?restype=container", &auth, "").await;
    assert_eq!(reply.header("x-ms-has-legal-hold"), "true");
    let shown = client.arm(Method::GET, &container(""), None).await.json();
    assert_eq!(shown["properties"]["hasLegalHold"], true);
    assert_eq!(shown["properties"]["legalHold"]["tags"][0]["tag"], "case123");

    // Clearing only some tags keeps the hold.
    let reply = client.arm(Method::POST, &container("/clearLegalHold"), Some(json!({"tags": ["case123"]}))).await;
    assert_eq!(reply.json()["hasLegalHold"], true);
    let reply = client.arm(Method::POST, &container("/clearLegalHold"), Some(json!({"tags": ["AUDIT2026"]}))).await;
    assert_eq!(reply.json(), json!({"hasLegalHold": false, "tags": []}));
    assert_eq!(client.send(Method::PUT, host, "/records/a.txt", &put, "v2").await.status, StatusCode::CREATED);

    // A retention policy protects blobs younger than its period.
    let policy = container("/immutabilityPolicies/default");
    assert_eq!(client.arm(Method::GET, &policy, None).await.status, StatusCode::NOT_FOUND);
    let reply = client.arm(Method::PUT, &policy, Some(json!({"properties": {"immutabilityPeriodSinceCreationInDays": 0}}))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = client.arm(Method::PUT, &policy, Some(json!({"properties": {"immutabilityPeriodSinceCreationInDays": 3}}))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let created = reply.json();
    assert_eq!(created["properties"]["state"], "Unlocked");
    let etag = created["etag"].as_str().unwrap().to_string();
    assert_eq!(reply.header("etag"), etag, "the entity tag is also the ETag header");
    let reply = client.send(Method::DELETE, host, "/records/a.txt", &auth, "").await;
    assert_eq!(reply.header("x-ms-error-code"), "BlobImmutableDueToPolicy");

    // Locking needs the current entity tag, and a locked policy only grows.
    let lock = container("/immutabilityPolicies/default/lock");
    assert_eq!(client.arm(Method::POST, &lock, None).await.status, StatusCode::BAD_REQUEST);
    let stale = [("if-match", "\"0x0\"")];
    let token = format!("Bearer {}", client.token(crate::support::ARM_SCOPE).await);
    let arm_with = |method: Method, path: String, headers: Vec<(&'static str, String)>, body: Value| {
        let token = token.clone();
        let client = &client;
        async move {
            let mut all = vec![("authorization", token.as_str()), ("content-type", "application/json")];
            all.extend(headers.iter().map(|(k, v)| (*k, v.as_str())));
            client.send(method, "localhost:10443", &path, &all, body.to_string()).await
        }
    };
    let reply = arm_with(Method::POST, lock.clone(), vec![("if-match", stale[0].1.to_string())], Value::Null).await;
    assert_eq!(reply.status, StatusCode::PRECONDITION_FAILED);
    let reply = arm_with(Method::POST, lock.clone(), vec![("if-match", etag.clone())], Value::Null).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["properties"]["state"], "Locked");
    let locked_etag = reply.json()["etag"].as_str().unwrap().to_string();
    assert_ne!(locked_etag, etag);

    let delete = arm_with(Method::DELETE, policy.clone(), vec![("if-match", locked_etag.clone())], Value::Null).await;
    assert_eq!(delete.status, StatusCode::CONFLICT, "a locked policy cannot be deleted");
    let update = client.arm(Method::PUT, &policy, Some(json!({"properties": {"immutabilityPeriodSinceCreationInDays": 1}}))).await;
    assert_eq!(update.status, StatusCode::CONFLICT, "a locked policy can only be extended");
    let extend = container("/immutabilityPolicies/default/extend");
    let shorter = arm_with(Method::POST, extend.clone(), vec![("if-match", locked_etag.clone())], json!({"properties": {"immutabilityPeriodSinceCreationInDays": 2}})).await;
    assert_eq!(shorter.status, StatusCode::BAD_REQUEST);
    let longer = arm_with(Method::POST, extend, vec![("if-match", locked_etag)], json!({"properties": {"immutabilityPeriodSinceCreationInDays": 30}})).await;
    assert_eq!(longer.status, StatusCode::OK, "{}", longer.text());
    assert_eq!(longer.json()["properties"]["immutabilityPeriodSinceCreationInDays"], 30);

    let shown = client.arm(Method::GET, &container(""), None).await.json();
    assert_eq!(shown["properties"]["hasImmutabilityPolicy"], true);
    let history: Vec<&str> = shown["properties"]["immutabilityPolicy"]["updateHistory"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["update"].as_str().unwrap())
        .collect();
    assert_eq!(history, ["put", "lock", "extend"]);
    assert_eq!(client.arm(Method::DELETE, &container(""), None).await.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn container_leases() {
    let client = management_client().await;
    let host = "mgmtacct.blob.core.azure.localhost:10443";
    let bearer = format!("Bearer {}", client.token(STORAGE_SCOPE).await);
    let container = |rest: &str| child(&format!("blobServices/default/containers/leased{rest}"));
    client.arm(Method::PUT, &container(""), Some(json!({}))).await;
    let lease = |body: Value| {
        let client = &client;
        let path = container("/lease");
        async move { client.arm(Method::POST, &path, Some(body)).await }
    };

    assert_eq!(lease(json!({"action": "Acquire", "leaseDuration": 5})).await.status, StatusCode::BAD_REQUEST);
    let reply = lease(json!({"action": "Acquire", "leaseDuration": -1})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let id = reply.json()["leaseId"].as_str().unwrap().to_string();
    assert_eq!(lease(json!({"action": "Acquire"})).await.status, StatusCode::CONFLICT);
    let shown = client.arm(Method::GET, &container(""), None).await.json();
    assert_eq!(shown["properties"]["leaseState"], "Leased");
    assert_eq!(shown["properties"]["leaseStatus"], "Locked");
    assert_eq!(shown["properties"]["leaseDuration"], "Infinite");

    // A leased container is deleted only with its lease id.
    let reply = client.send(Method::DELETE, host, "/leased?restype=container", &[("authorization", &bearer)], "").await;
    assert_eq!(reply.header("x-ms-error-code"), "LeaseIdMissing");
    let wrong = [("authorization", bearer.as_str()), ("x-ms-lease-id", "00000000-0000-0000-0000-000000000000")];
    let reply = client.send(Method::DELETE, host, "/leased?restype=container", &wrong, "").await;
    assert_eq!(reply.header("x-ms-error-code"), "LeaseIdMismatchWithContainerOperation");
    assert_eq!(client.arm(Method::DELETE, &container(""), None).await.status, StatusCode::PRECONDITION_FAILED);
    let reply = client.send(Method::GET, host, "/leased?restype=container", &[("authorization", &bearer)], "").await;
    assert_eq!(reply.header("x-ms-lease-state"), "leased");

    let changed = "11111111-2222-3333-4444-555555555555";
    let reply = lease(json!({"action": "Change", "leaseId": id, "proposedLeaseId": changed})).await;
    assert_eq!(reply.json()["leaseId"], changed);
    assert_eq!(lease(json!({"action": "Renew", "leaseId": id})).await.status, StatusCode::CONFLICT);
    let reply = lease(json!({"action": "Break", "breakPeriod": 0})).await;
    assert_eq!(reply.json()["leaseTimeSeconds"], "0");
    assert_eq!(client.arm(Method::GET, &container(""), None).await.json()["properties"]["leaseState"], "Broken");

    let ok = [("authorization", bearer.as_str())];
    assert_eq!(client.send(Method::DELETE, host, "/leased?restype=container", &ok, "").await.status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn file_share_leases_and_user_delegation_key_revocation() {
    let client = management_client().await;
    let share = child("fileServices/default/shares/docs");
    client.arm(Method::PUT, &share, Some(json!({}))).await;
    let lease = child("fileServices/default/shares/docs/lease");

    let reply = client.arm(Method::POST, &lease, Some(json!({"action": "Acquire", "leaseDuration": 30}))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let id = reply.json()["leaseId"].as_str().unwrap().to_string();
    let shown = client.arm(Method::GET, &share, None).await.json();
    assert_eq!(shown["properties"]["leaseState"], "leased");
    assert_eq!(shown["properties"]["leaseDuration"], "fixed");
    assert_eq!(client.arm(Method::DELETE, &share, None).await.status, StatusCode::PRECONDITION_FAILED);

    // A finite lease cannot be broken later than it would have expired.
    let reply = client.arm(Method::POST, &lease, Some(json!({"action": "Break", "breakPeriod": 60}))).await;
    assert!(reply.json()["leaseTimeSeconds"].as_str().unwrap().parse::<u64>().unwrap() <= 30);
    assert_eq!(client.arm(Method::GET, &share, None).await.json()["properties"]["leaseState"], "breaking");
    let reply = client.arm(Method::POST, &lease, Some(json!({"action": "Release", "leaseId": id}))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(client.arm(Method::GET, &share, None).await.json()["properties"]["leaseState"], "available");
    assert_eq!(client.arm(Method::DELETE, &share, None).await.status, StatusCode::OK);

    let revoke = child("revokeUserDelegationKeys");
    let reply = client.arm(Method::POST, &revoke, None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.is_empty());
}
