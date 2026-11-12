use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use http::{Method, StatusCode};
use serde_json::json;
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
