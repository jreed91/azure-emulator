//! Small HTTP helpers shared by every service.

use std::collections::BTreeMap;

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use http::{header, HeaderMap, StatusCode};
use serde_json::Value;

/// Largest request body the emulator buffers (256 MiB).
pub const MAX_BODY_BYTES: usize = 256 * 1024 * 1024;

/// A JSON response with the given status.
pub fn json_response(status: StatusCode, body: &Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body.to_string(),
    )
        .into_response()
}

/// Decodes a query string into a map. Keys keep their case; later duplicates win.
pub fn parse_query(query: Option<&str>) -> BTreeMap<String, String> {
    parse_form(query.unwrap_or(""))
}

/// Decodes `application/x-www-form-urlencoded` content.
pub fn parse_form(input: &str) -> BTreeMap<String, String> {
    input
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key, true), percent_decode(value, true))
        })
        .collect()
}

/// Percent-decodes `input`; `plus_as_space` applies form/query rules.
pub fn percent_decode(input: &str, plus_as_space: bool) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' if plus_as_space => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The request's hostname, lowercased and without a port.
pub fn request_host(headers: &HeaderMap, uri: &http::Uri) -> String {
    let raw = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| uri.authority().map(|a| a.as_str()))
        .unwrap_or("");
    let host = match raw.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => raw,
    };
    host.to_ascii_lowercase()
}

/// Reads the whole request body, bounded by [`MAX_BODY_BYTES`].
pub async fn read_body(body: Body) -> Result<axum::body::Bytes, Response> {
    axum::body::to_bytes(body, MAX_BODY_BYTES).await.map_err(|_| {
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body exceeds the emulator's limit",
        )
            .into_response()
    })
}

/// A fresh request id, as Azure services return in `x-ms-request-id`.
pub fn request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The current UTC time as Azure renders it: seven fractional digits and a `Z`.
pub fn iso_timestamp() -> String {
    let now = chrono::Utc::now();
    let nanos = now.format("%f").to_string();
    format!("{}.{}Z", now.format("%Y-%m-%dT%H:%M:%S"), &nanos[..7])
}
