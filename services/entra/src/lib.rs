//! Microsoft Entra ID token service.
//!
//! Serves what an Azure SDK credential needs to obtain an access token from the
//! emulator instead of `login.microsoftonline.com`: OpenID Connect discovery,
//! instance discovery, the signing keys, the client-credentials token endpoints
//! (v1 `resource` and v2 `scope`), and the managed-identity (IMDS) token endpoint.
//!
//! The emulator has no application registry yet: any client id is accepted as long
//! as the request carries a credential for it. Do not use this to test security.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::response::Response;
use base64::Engine as _;
use emulator_core::auth::{self, TOKEN_LIFETIME_SECS};
use emulator_core::http_util::{json_response, parse_form, parse_query, read_body};
use emulator_core::Engine;
use http::{header, Method, Request, StatusCode};
use serde_json::{json, Value};

/// Client id the managed-identity endpoint issues tokens for.
pub const MANAGED_IDENTITY_CLIENT_ID: &str = "22222222-2222-2222-2222-222222222222";

fn segments(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Whether `path` belongs to the token service rather than Resource Manager.
pub fn matches(path: &str) -> bool {
    let segments = segments(path);
    let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
    matches!(
        segments.as_slice(),
        ["common", "discovery", "instance"]
            | ["metadata", "identity", "oauth2", "token"]
            | [_, "oauth2", "token"]
            | [_, "oauth2", "v2.0", "token"]
            | [_, "discovery", "keys"]
            | [_, "discovery", "v2.0", "keys"]
            | [_, ".well-known", "openid-configuration"]
            | [_, "v2.0", ".well-known", "openid-configuration"]
    )
}

fn oauth_error(status: StatusCode, error: &str, code: u32, description: &str) -> Response {
    json_response(
        status,
        &json!({
            "error": error,
            "error_description": format!("AADSTS{code}: {description}"),
            "error_codes": [code],
        }),
    )
}

fn epoch_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Serves one token-service request. Call only for paths [`matches`] accepts.
pub async fn handle(engine: Arc<Engine>, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let config = engine.config();
    let base = config.management_endpoint();
    let authority = base.split_once("://").map(|(_, a)| a).unwrap_or(&base).to_string();
    let query = parse_query(parts.uri.query());
    let path = segments(parts.uri.path());
    let path: Vec<&str> = path.iter().map(String::as_str).collect();

    if path == ["metadata", "identity", "oauth2", "token"] {
        return managed_identity_token(&engine, &parts, &query);
    }
    if path == ["common", "discovery", "instance"] {
        // The tenant is whatever the authorization endpoint being validated names.
        let tenant = query
            .get("authorization_endpoint")
            .and_then(|url| url.split_once("://"))
            .and_then(|(_, rest)| rest.split('/').nth(1))
            .filter(|t| !t.is_empty())
            .unwrap_or(&config.tenant_id);
        return json_response(
            StatusCode::OK,
            &json!({
                "tenant_discovery_endpoint":
                    format!("{base}/{tenant}/v2.0/.well-known/openid-configuration"),
                "api-version": "1.1",
                "metadata": [{
                    "preferred_network": authority,
                    "preferred_cache": authority,
                    "aliases": [authority],
                }],
            }),
        );
    }

    let tenant = path[0];
    let known = ["common", "organizations", &config.tenant_id.to_ascii_lowercase()]
        .contains(&tenant);
    if !known {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            90002,
            &format!("Tenant '{tenant}' not found. Check to make sure you have the correct tenant ID and are signing into the correct cloud."),
        );
    }

    match (&parts.method, &path[1..]) {
        (&Method::GET, [".well-known", "openid-configuration"])
        | (&Method::GET, ["v2.0", ".well-known", "openid-configuration"]) => json_response(
            StatusCode::OK,
            &json!({
                "issuer": config.issuer(),
                "authorization_endpoint": format!("{base}/{tenant}/oauth2/v2.0/authorize"),
                "token_endpoint": format!("{base}/{tenant}/oauth2/v2.0/token"),
                "jwks_uri": format!("{base}/{tenant}/discovery/v2.0/keys"),
                "token_endpoint_auth_methods_supported":
                    ["client_secret_post", "client_secret_basic", "private_key_jwt"],
                "response_types_supported": ["code"],
                "subject_types_supported": ["pairwise"],
                "id_token_signing_alg_values_supported": ["RS256"],
            }),
        ),
        (&Method::GET, ["discovery", "keys"]) | (&Method::GET, ["discovery", "v2.0", "keys"]) => {
            json_response(StatusCode::OK, &auth::jwks())
        }
        (&Method::POST, ["oauth2", "token"]) | (&Method::POST, ["oauth2", "v2.0", "token"]) => {
            let bytes = match read_body(body).await {
                Ok(bytes) => bytes,
                Err(response) => return response,
            };
            let form = parse_form(&String::from_utf8_lossy(&bytes));
            client_credentials_token(&engine, &parts, &form, path.len() == 4)
        }
        _ => oauth_error(
            StatusCode::NOT_IMPLEMENTED,
            "not_implemented",
            0,
            "The emulator does not implement this endpoint yet.",
        ),
    }
}

/// Client id and whether a credential accompanies it, from the form or Basic auth.
fn client_identity(
    parts: &http::request::Parts,
    form: &std::collections::BTreeMap<String, String>,
) -> (Option<String>, bool) {
    let basic = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v).ok())
        .and_then(|v| String::from_utf8(v).ok())
        .and_then(|v| v.split_once(':').map(|(id, secret)| (id.to_string(), secret.to_string())));
    let has_credential = ["client_secret", "client_assertion"]
        .iter()
        .any(|k| form.get(*k).is_some_and(|v| !v.is_empty()))
        || basic.as_ref().is_some_and(|(_, secret)| !secret.is_empty());
    let client_id = form.get("client_id").cloned().or(basic.map(|(id, _)| id));
    (client_id.filter(|id| !id.is_empty()), has_credential)
}

fn client_credentials_token(
    engine: &Engine,
    parts: &http::request::Parts,
    form: &std::collections::BTreeMap<String, String>,
    v2: bool,
) -> Response {
    let grant = form.get("grant_type").map(String::as_str).unwrap_or("");
    if grant != "client_credentials" {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            70003,
            &format!("The app requested an unsupported grant type '{grant}'. The emulator supports 'client_credentials' only."),
        );
    }
    let (client_id, has_credential) = client_identity(parts, form);
    let Some(client_id) = client_id else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            900144,
            "The request body must contain the following parameter: 'client_id'.",
        );
    };
    if !has_credential {
        return oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            7000216,
            "'client_assertion', 'client_secret' or 'request' is required for the 'client_credentials' grant type.",
        );
    }
    // v2 names the resource as `<resource>/.default`; v1 names it directly.
    let audience = if v2 {
        form.get("scope").map(|scope| {
            let first = scope.split_whitespace().next().unwrap_or("");
            first.strip_suffix("/.default").unwrap_or(first).to_string()
        })
    } else {
        form.get("resource").cloned()
    };
    let Some(audience) = audience.filter(|a| !a.is_empty()) else {
        let field = if v2 { "scope" } else { "resource" };
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            90014,
            &format!("The required field '{field}' is missing from the credential. Ensure that you have all the necessary parameters for the login request."),
        );
    };
    let token = auth::issue_token(engine.config(), &audience, &client_id);
    let expires_in = TOKEN_LIFETIME_SECS - 1;
    let body: Value = if v2 {
        json!({
            "token_type": "Bearer",
            "expires_in": expires_in,
            "ext_expires_in": expires_in,
            "access_token": token,
        })
    } else {
        let now = epoch_now();
        json!({
            "token_type": "Bearer",
            "expires_in": expires_in.to_string(),
            "ext_expires_in": expires_in.to_string(),
            "expires_on": (now + expires_in).to_string(),
            "not_before": now.to_string(),
            "resource": audience,
            "access_token": token,
        })
    };
    json_response(StatusCode::OK, &body)
}

fn managed_identity_token(
    engine: &Engine,
    parts: &http::request::Parts,
    query: &std::collections::BTreeMap<String, String>,
) -> Response {
    let bad_request = |description: &str| {
        json_response(
            StatusCode::BAD_REQUEST,
            &json!({"error": "invalid_request", "error_description": description}),
        )
    };
    let has_metadata_header = parts
        .headers
        .get("metadata")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    if !has_metadata_header {
        return bad_request("Required metadata header not specified");
    }
    let Some(resource) = query.get("resource").filter(|r| !r.is_empty()) else {
        return bad_request("Required query variable 'resource' is missing");
    };
    let client_id = query
        .get("client_id")
        .map(String::as_str)
        .unwrap_or(MANAGED_IDENTITY_CLIENT_ID);
    let token = auth::issue_token(engine.config(), resource, client_id);
    let now = epoch_now();
    let expires_in = TOKEN_LIFETIME_SECS - 1;
    json_response(
        StatusCode::OK,
        &json!({
            "access_token": token,
            "client_id": client_id,
            "expires_in": expires_in.to_string(),
            "expires_on": (now + expires_in).to_string(),
            "ext_expires_in": expires_in.to_string(),
            "not_before": now.to_string(),
            "resource": resource,
            "token_type": "Bearer",
        }),
    )
}
