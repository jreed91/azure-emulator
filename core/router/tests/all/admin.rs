//! What the router does for every host: the readiness endpoint, and a
//! handler's panic answered as a 500 instead of a dropped connection.

use axum::body::Body;
use axum::response::Response;
use azure_emulator::catch_panic;
use http::{Method, Request, StatusCode};
use tower::ServiceExt;

use crate::support::Client;

#[tokio::test]
async fn health_answers_on_every_host_without_authentication() {
    let client = Client::new();
    let suffix = client.config().dns_suffix.clone();
    for host in [
        "localhost:10443".to_string(),
        format!("account.blob.core.{suffix}"),
        format!("vault.vault.{suffix}"),
        format!("ns.servicebus.{suffix}"),
        format!("db.documents.{suffix}"),
        "anything.example".to_string(),
    ] {
        let reply = client.send(Method::GET, &host, "/__admin/health", &[], Body::empty()).await;
        assert_eq!(reply.status, StatusCode::OK, "{host}: {}", reply.text());
        let body = reply.json();
        assert_eq!(body["status"], "ok", "{host}");
        assert_eq!(body["version"], azure_emulator::VERSION, "{host}");
        assert!(reply.header("content-type").starts_with("application/json"));
    }
    let head = client.send(Method::HEAD, "localhost:10443", "/__admin/health?probe=1", &[], Body::empty()).await;
    assert_eq!(head.status, StatusCode::OK);

    // Other paths still need a token.
    let other = client.send(Method::GET, "localhost:10443", "/__admin/other", &[], Body::empty()).await;
    assert_eq!(other.status, StatusCode::UNAUTHORIZED, "{}", other.text());
}

async fn body_json(response: Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("readable body");
    serde_json::from_slice(&bytes).expect("JSON")
}

#[tokio::test]
async fn a_panic_becomes_an_azure_style_500() {
    let response = catch_panic("GET /boom", async { panic!("secret detail 1234") }).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "InternalServerError");
    assert!(!body.to_string().contains("secret detail"), "the payload is not sent: {body}");

    // A panic after the handler has yielded is caught too.
    let response = catch_panic("GET /later", async {
        tokio::task::yield_now().await;
        let fail = true;
        if fail {
            panic!("after an await");
        }
        Response::new(Body::empty())
    })
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // A handler that does not panic is answered as it answers.
    let response = catch_panic("GET /fine", async {
        let mut response = Response::new(Body::from("fine"));
        *response.status_mut() = StatusCode::ACCEPTED;
        response
    })
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn a_router_keeps_serving_after_a_handler_panics() {
    // A router whose handler panics on one path, wrapped as the emulator's is.
    async fn handler(request: Request<Body>) -> Response {
        if request.uri().path() == "/boom" {
            panic!("deliberate");
        }
        Response::new(Body::from("ok"))
    }
    let router = axum::Router::new().fallback(|request: Request<Body>| catch_panic("test", handler(request)));
    let send = |path: &'static str| {
        let router = router.clone();
        async move { router.oneshot(Request::get(path).body(Body::empty()).unwrap()).await.unwrap() }
    };
    assert_eq!(send("/boom").await.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(send("/fine").await.status(), StatusCode::OK);
    assert_eq!(send("/boom").await.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // And the emulator's own router still answers after that.
    let client = Client::new();
    let reply = client.send(Method::GET, "localhost:10443", "/__admin/health", &[], Body::empty()).await;
    assert_eq!(reply.status, StatusCode::OK);
}
