use http::{Method, StatusCode};

use crate::support::{Client, ARM_SCOPE};

const HOST: &str = "localhost:10443";

#[tokio::test]
async fn discovery_documents_point_back_at_the_emulator() {
    let client = Client::new();
    let tenant = client.config().tenant_id.clone();
    let reply = client
        .send(Method::GET, HOST, &format!("/{tenant}/v2.0/.well-known/openid-configuration"), &[], "")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let document = reply.json();
    assert_eq!(
        document["token_endpoint"],
        format!("https://localhost:10443/{tenant}/oauth2/v2.0/token")
    );

    let reply = client.send(Method::GET, HOST, &format!("/{tenant}/discovery/v2.0/keys"), &[], "").await;
    assert_eq!(reply.json()["keys"][0]["kty"], "RSA");

    let path = format!(
        "/common/discovery/instance?api-version=1.1&authorization_endpoint=https%3A%2F%2Flocalhost%3A10443%2F{tenant}%2Foauth2%2Fv2.0%2Fauthorize"
    );
    let reply = client.send(Method::GET, HOST, &path, &[], "").await;
    assert_eq!(
        reply.json()["tenant_discovery_endpoint"],
        format!("https://localhost:10443/{tenant}/v2.0/.well-known/openid-configuration")
    );
}

#[tokio::test]
async fn client_credentials_requires_a_credential_and_a_scope() {
    let client = Client::new();
    let path = format!("/{}/oauth2/v2.0/token", client.config().tenant_id);
    let form = [("content-type", "application/x-www-form-urlencoded")];

    let reply = client
        .send(Method::POST, HOST, &path, &form, "grant_type=client_credentials&client_id=app&scope=x%2F.default")
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.json()["error"], "invalid_client");

    let reply = client
        .send(Method::POST, HOST, &path, &form, "grant_type=client_credentials&client_id=app&client_secret=s")
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.json()["error"], "invalid_request");

    let reply = client
        .send(Method::POST, HOST, &path, &form, "grant_type=password&client_id=app")
        .await;
    assert_eq!(reply.json()["error"], "unsupported_grant_type");

    let reply = client
        .send(Method::POST, HOST, "/not-a-tenant/oauth2/v2.0/token", &form, "grant_type=client_credentials")
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.json()["error_description"].as_str().unwrap().starts_with("AADSTS90002"));
}

#[tokio::test]
async fn v1_endpoint_takes_a_resource() {
    let client = Client::new();
    let path = format!("/{}/oauth2/token", client.config().tenant_id);
    let reply = client
        .send(
            Method::POST,
            HOST,
            &path,
            &[("content-type", "application/x-www-form-urlencoded")],
            "grant_type=client_credentials&client_id=app&client_secret=s&resource=https%3A%2F%2Fmanagement.azure.com%2F",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let body = reply.json();
    assert_eq!(body["resource"], "https://management.azure.com/");
    let authorization = format!("Bearer {}", body["access_token"].as_str().unwrap());
    let reply = client
        .send(Method::GET, HOST, "/subscriptions?api-version=2022-12-01", &[("authorization", &authorization)], "")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn managed_identity_endpoint_needs_the_metadata_header() {
    let client = Client::new();
    let path = "/metadata/identity/oauth2/token?api-version=2018-02-01&resource=https%3A%2F%2Fmanagement.azure.com%2F";
    let reply = client.send(Method::GET, HOST, path, &[], "").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = client.send(Method::GET, HOST, path, &[("metadata", "true")], "").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["token_type"], "Bearer");
}

#[tokio::test]
async fn tokens_are_bound_to_their_audience() {
    let client = Client::new();
    let wrong = format!("Bearer {}", client.token("https://vault.azure.net/.default").await);
    let reply = client
        .send(Method::GET, HOST, "/subscriptions?api-version=2022-12-01", &[("authorization", &wrong)], "")
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.error_code(), "InvalidAuthenticationTokenAudience");

    let right = format!("Bearer {}", client.token(ARM_SCOPE).await);
    let reply = client
        .send(Method::GET, HOST, "/subscriptions?api-version=2022-12-01", &[("authorization", &right)], "")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
}
