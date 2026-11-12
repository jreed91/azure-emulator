//! An in-process client for one emulator instance.

use axum::body::Body;
use azure_emulator::{Config, Emulator};
use http::{HeaderMap, Method, Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

pub const SUB: &str = "/subscriptions/00000000-0000-0000-0000-000000000000";
pub const ARM_SCOPE: &str = "https://management.azure.com/.default";
pub const STORAGE_SCOPE: &str = "https://storage.azure.com/.default";

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn header(&self, name: &str) -> &str {
        self.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
    }

    pub fn error_code(&self) -> String {
        self.json()["error"]["code"].as_str().unwrap_or("").to_string()
    }
}

pub struct Client {
    pub emulator: Emulator,
}

impl Client {
    pub fn new() -> Self {
        Self {
            emulator: Emulator::new(Config::default()),
        }
    }

    pub fn config(&self) -> &Config {
        self.emulator.config()
    }

    /// Sends one request. `host` is the `Host` header: what selects the service.
    pub async fn send(
        &self,
        method: Method,
        host: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: impl Into<Body>,
    ) -> Reply {
        let mut request = Request::builder().method(method).uri(path).header("host", host);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = self
            .emulator
            .router()
            .oneshot(request.body(body.into()).expect("valid request"))
            .await
            .expect("the router is infallible");
        let (parts, body) = response.into_parts();
        let body = axum::body::to_bytes(body, usize::MAX).await.expect("readable body");
        Reply {
            status: parts.status,
            headers: parts.headers,
            body: body.to_vec(),
        }
    }

    /// A client-credentials access token for `scope` from the v2 token endpoint.
    pub async fn token(&self, scope: &str) -> String {
        let form = format!(
            "grant_type=client_credentials&client_id=test-app&client_secret=s3cret&scope={}",
            scope.replace(':', "%3A").replace('/', "%2F")
        );
        let path = format!("/{}/oauth2/v2.0/token", self.config().tenant_id);
        let reply = self
            .send(Method::POST, "localhost:10443", &path, &[("content-type", "application/x-www-form-urlencoded")], form)
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        reply.json()["access_token"].as_str().expect("access_token").to_string()
    }

    /// An authenticated Resource Manager request with a JSON body.
    pub async fn arm(&self, method: Method, path: &str, body: Option<Value>) -> Reply {
        let token = self.token(ARM_SCOPE).await;
        let authorization = format!("Bearer {token}");
        let body = body.map(|b| Body::from(b.to_string())).unwrap_or_else(Body::empty);
        self.send(
            method,
            "localhost:10443",
            path,
            &[("authorization", &authorization), ("content-type", "application/json")],
            body,
        )
        .await
    }

    pub async fn create_group(&self, name: &str) {
        let path = format!("{SUB}/resourcegroups/{name}?api-version=2021-04-01");
        let reply = self.arm(Method::PUT, &path, Some(serde_json::json!({"location": "eastus"}))).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    }
}
