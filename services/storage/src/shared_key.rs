//! Shared Key authorization for the Blob service (the `SharedKey` scheme of service
//! version 2009-09-19 and later).
//!
//! The client signs a canonical form of the request with an account key; the service
//! rebuilds that string from the request it received and compares signatures.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use emulator_core::http_util::percent_decode;
use hmac::{Hmac, Mac};
use http::{HeaderMap, Method, Uri};
use sha2::Sha256;

/// The string a client signs for this request.
pub(crate) fn string_to_sign(method: &Method, uri: &Uri, headers: &HeaderMap, account: &str) -> String {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("");
    // An absent or zero Content-Length is signed as the empty string.
    let content_length = match header("content-length") {
        "0" => "",
        other => other,
    };
    let mut out = format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
        method.as_str(),
        header("content-encoding"),
        header("content-language"),
        content_length,
        header("content-md5"),
        header("content-type"),
        header("date"),
        header("if-modified-since"),
        header("if-match"),
        header("if-none-match"),
        header("if-unmodified-since"),
        header("range"),
    );

    let mut ms_headers: Vec<(String, String)> = headers
        .iter()
        .filter(|(name, _)| name.as_str().starts_with("x-ms-"))
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or("").trim().to_string(),
            )
        })
        .collect();
    ms_headers.sort();
    for (name, value) in ms_headers {
        out.push_str(&format!("{name}:{value}\n"));
    }

    out.push_str(&format!("/{account}{}", uri.path()));
    let mut params: Vec<(String, Vec<String>)> = Vec::new();
    for pair in uri.query().unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let name = percent_decode(name, true).to_ascii_lowercase();
        let value = percent_decode(value, true);
        match params.iter_mut().find(|(n, _)| *n == name) {
            Some((_, values)) => values.push(value),
            None => params.push((name, vec![value])),
        }
    }
    params.sort();
    for (name, mut values) in params {
        values.sort();
        out.push_str(&format!("\n{name}:{}", values.join(",")));
    }
    out
}

/// Whether `signature` is the HMAC-SHA256 of `string_to_sign` under the base64 `key`.
pub(crate) fn verify(string_to_sign: &str, key: &str, signature: &str) -> bool {
    let (Ok(key), Ok(signature)) = (STANDARD.decode(key), STANDARD.decode(signature)) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(&key) else {
        return false;
    };
    mac.update(string_to_sign.as_bytes());
    mac.verify_slice(&signature).is_ok()
}

fn table_resource(uri: &Uri, account: &str) -> String {
    let mut resource = format!("/{account}{}", uri.path());
    for pair in uri.query().unwrap_or("").split('&') {
        if let Some(value) = pair.strip_prefix("comp=") {
            resource.push_str(&format!("?comp={}", percent_decode(value, true)));
        }
    }
    resource
}

/// The string a client signs for a Table request with `SharedKey`.
pub(crate) fn table_string_to_sign(
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    account: &str,
) -> String {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    };
    let date = match header("x-ms-date") {
        "" => header("date"),
        date => date,
    };
    format!(
        "{}\n{}\n{}\n{}\n{}",
        method.as_str(),
        header("content-md5"),
        header("content-type"),
        date,
        table_resource(uri, account)
    )
}

/// The string a client signs for a Table request with `SharedKeyLite`.
pub(crate) fn table_lite_string_to_sign(uri: &Uri, headers: &HeaderMap, account: &str) -> String {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    };
    let date = match header("x-ms-date") {
        "" => header("date"),
        date => date,
    };
    format!("{date}\n{}", table_resource(uri, account))
}
