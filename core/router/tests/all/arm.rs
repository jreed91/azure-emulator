use http::{Method, StatusCode};
use serde_json::json;

use crate::support::{Client, SUB};

#[tokio::test]
async fn requests_need_a_token_and_an_api_version() {
    let client = Client::new();
    let reply = client.send(Method::GET, "localhost:10443", "/subscriptions?api-version=2022-12-01", &[], "").await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.error_code(), "AuthenticationFailed");
    assert!(reply.header("www-authenticate").starts_with("Bearer authorization_uri="));

    let reply = client
        .send(Method::GET, "localhost:10443", "/subscriptions?api-version=2022-12-01", &[("authorization", "Bearer nope")], "")
        .await;
    assert_eq!(reply.error_code(), "InvalidAuthenticationToken");

    let reply = client.arm(Method::GET, "/subscriptions", None).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.error_code(), "MissingApiVersionParameter");

    let reply = client.arm(Method::GET, "/subscriptions?api-version=latest", None).await;
    assert_eq!(reply.error_code(), "InvalidApiVersionParameter");
}

#[tokio::test]
async fn subscription_and_metadata() {
    let client = Client::new();
    let reply = client.arm(Method::GET, "/subscriptions?api-version=2022-12-01", None).await;
    assert_eq!(reply.json()["value"][0]["subscriptionId"], client.config().subscription_id);

    let reply = client
        .arm(Method::GET, "/subscriptions/99999999-9999-9999-9999-999999999999?api-version=2022-12-01", None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.error_code(), "SubscriptionNotFound");

    // The cloud metadata document is readable before signing in.
    let reply = client
        .send(Method::GET, "localhost:10443", "/metadata/endpoints?api-version=2022-09-01", &[], "")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let cloud = &reply.json()[0];
    assert_eq!(cloud["resourceManager"], "https://localhost:10443/");
    assert_eq!(cloud["suffixes"]["storage"], "core.azure.localhost:10443");
}

#[tokio::test]
async fn resource_group_lifecycle() {
    let client = Client::new();
    let path = format!("{SUB}/resourcegroups/Demo-RG?api-version=2021-04-01");

    let reply = client.arm(Method::PUT, &path, Some(json!({"tags": {"a": "b"}}))).await;
    assert_eq!(reply.error_code(), "LocationRequired");

    let reply = client
        .arm(Method::PUT, &path, Some(json!({"location": "East US", "tags": {"env": "test"}})))
        .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let group = reply.json();
    assert_eq!(group["id"], format!("{SUB}/resourceGroups/Demo-RG"));
    assert_eq!(group["location"], "eastus");
    assert_eq!(group["properties"]["provisioningState"], "Succeeded");

    // Names are case-insensitive, and a second PUT is an update.
    let lower = format!("{SUB}/resourceGroups/demo-rg?api-version=2021-04-01");
    let reply = client.arm(Method::PUT, &lower, Some(json!({"location": "eastus"}))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["name"], "Demo-RG");
    assert!(reply.json().get("tags").is_none(), "PUT replaces tags");

    let reply = client.arm(Method::PUT, &lower, Some(json!({"location": "westus"}))).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.error_code(), "InvalidResourceGroupLocation");

    let reply = client.arm(Method::PATCH, &lower, Some(json!({"tags": {"k": "v"}}))).await;
    assert_eq!(reply.json()["tags"]["k"], "v");

    let reply = client.arm(Method::HEAD, &lower, None).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);

    let reply = client.arm(Method::GET, &format!("{SUB}/resourcegroups?api-version=2021-04-01"), None).await;
    assert_eq!(reply.json()["value"].as_array().unwrap().len(), 1);

    let reply = client.arm(Method::DELETE, &lower, None).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    let location = reply.header("location").to_string();
    let poll = location.strip_prefix("https://localhost:10443").expect("status URL on the emulator");
    assert_eq!(client.arm(Method::GET, poll, None).await.status, StatusCode::OK);

    let reply = client.arm(Method::GET, &lower, None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.error_code(), "ResourceGroupNotFound");
    assert_eq!(client.arm(Method::DELETE, &lower, None).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unknown_providers_and_types_are_refused() {
    let client = Client::new();
    client.create_group("rg").await;
    let reply = client
        .arm(Method::GET, &format!("{SUB}/resourceGroups/rg/providers/Microsoft.Nope/things/x?api-version=2021-04-01"), None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.error_code(), "InvalidResourceNamespace");

    let reply = client
        .arm(Method::GET, &format!("{SUB}/resourceGroups/rg/providers/Microsoft.Storage/things/x?api-version=2021-04-01"), None)
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.error_code(), "InvalidResourceType");

    let reply = client.arm(Method::GET, &format!("{SUB}/providers/microsoft.storage?api-version=2021-04-01"), None).await;
    assert_eq!(reply.json()["namespace"], "Microsoft.Storage");

    // Anything unimplemented is an honest 501, never a made-up success.
    let reply = client.arm(Method::GET, &format!("{SUB}/locations?api-version=2022-12-01"), None).await;
    assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED);
}
