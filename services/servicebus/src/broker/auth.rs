//! Claims-based security (CBS): the tokens a client puts on the `$cbs` node, and
//! the rights they grant on the connection.
//!
//! A shared access signature is verified against the keys of the authorization
//! rule it names: the namespace's rule, or the addressed entity's. Its
//! signature, expiry and resource are all checked, and it grants the rule's
//! rights (`Send`, `Listen`; `Manage` implies both) on the resource it was
//! issued for and everything under it. An Entra ID token issued by the
//! emulator for Service Bus grants every right: role assignments are not
//! enforced anywhere in the emulator yet.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use emulator_core::arm::ArmState;
use emulator_core::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::ServiceBusState;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Right {
    Send,
    Listen,
}

/// What a token allows on this connection.
#[derive(Clone, Debug)]
pub(crate) struct Grant {
    namespace: String,
    /// Lowercased entity path prefix; empty for the whole namespace.
    path: String,
    send: bool,
    listen: bool,
    expires: u64,
}

impl Grant {
    /// The namespace this grant is for.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Whether this grant allows `right` on `path` in `namespace` now.
    pub fn allows(&self, namespace: &str, path: &str, right: Right) -> bool {
        let path = path.to_ascii_lowercase();
        let covered = self.path.is_empty() || path == self.path || path.starts_with(&format!("{}/", self.path));
        let has = match right {
            Right::Send => self.send,
            Right::Listen => self.listen,
        };
        has && covered && self.namespace.eq_ignore_ascii_case(namespace) && now() < self.expires
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `sb://host[:port]/path` (or `amqp`, `amqps`) as `(host, path)`.
fn split_host(uri: &str) -> Option<(String, String)> {
    let rest = uri.split_once("://").map(|(_, rest)| rest).unwrap_or(uri);
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.split(':').next()?.to_ascii_lowercase();
    (!host.is_empty()).then(|| (host, path.trim_matches('/').to_string()))
}

/// The namespace a client means by `host`. On the emulator's own domain
/// (`<namespace>.servicebus.<suffix>`) and for any host whose first label
/// names a namespace, that namespace; for any other host (`localhost`, a
/// container's name) the only namespace there is, so a connection string
/// can point at whatever name reaches the emulator.
pub(crate) fn namespace_for_host(engine: &Engine, host: &str) -> String {
    let first = host.split('.').next().unwrap_or(host).to_ascii_lowercase();
    let own_domain = format!(".servicebus.{}", engine.config().dns_suffix.to_ascii_lowercase());
    if host.ends_with(&own_domain) {
        return first;
    }
    let state = engine.state::<ArmState>();
    let state = state.lock();
    let mut namespaces = state
        .resources()
        .filter(|r| r.resource_type.eq_ignore_ascii_case("Microsoft.ServiceBus/namespaces"))
        .map(|r| r.name.to_ascii_lowercase());
    let names: Vec<String> = namespaces.by_ref().take(64).collect();
    if names.contains(&first) {
        return first;
    }
    match names.as_slice() {
        [only] => only.clone(),
        _ => first,
    }
}

/// The entity a token's path is for: a token for an entity's `$management`
/// node (as the JavaScript SDK's rule manager asks for) is for the entity.
fn entity_of(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    match lower.strip_suffix("/$management") {
        Some(_) => path[..path.len() - "/$management".len()].to_string(),
        None if lower == "$management" => String::new(),
        None => path.to_string(),
    }
}

/// `sb://host[:port]/path` (or `amqp`, `amqps`) as `(namespace, path)`.
pub(crate) fn split_uri(engine: &Engine, uri: &str) -> Option<(String, String)> {
    let (host, path) = split_host(uri)?;
    Some((namespace_for_host(engine, &host), path))
}

fn percent_decode(text: &str) -> String {
    emulator_core::http_util::percent_decode(text, true)
}

/// Verifies a CBS token for `audience`. `kind` is the token type the client
/// declared (`servicebus.windows.net:sastoken` or `jwt`).
pub(crate) fn verify(engine: &Engine, kind: &str, token: &str, audience: &str) -> Result<Grant, String> {
    let (namespace, audience_path) = split_uri(engine, audience).ok_or("The token's audience is not a Service Bus address.")?;
    let audience_path = entity_of(&audience_path);
    // The SDKs declare `jwt` for shared access signatures too, so the token's
    // own form decides how it is checked.
    let _ = kind;
    if !token.starts_with("SharedAccessSignature ") {
        let audiences = ["https://servicebus.azure.net".to_string(), format!("sb://{namespace}")];
        let claims = emulator_core::auth::verify_token(token, &audiences).map_err(|_| "The token is not valid for Service Bus.".to_string())?;
        return Ok(Grant { namespace, path: String::new(), send: true, listen: true, expires: claims.exp });
    }
    let fields = token.strip_prefix("SharedAccessSignature ").ok_or("The token is not a shared access signature.")?;
    let mut raw_resource = None;
    let (mut signature, mut expiry, mut key_name) = (None, None, None);
    for pair in fields.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        match name {
            "sr" => raw_resource = Some(value.to_string()),
            "sig" => signature = Some(percent_decode(value)),
            "se" => expiry = value.parse::<u64>().ok(),
            "skn" => key_name = Some(percent_decode(value)),
            _ => {}
        }
    }
    let (raw_resource, signature, expiry, key_name) = match (raw_resource, signature, expiry, key_name) {
        (Some(r), Some(s), Some(e), Some(k)) => (r, s, e, k),
        _ => return Err("The shared access signature is missing sr, sig, se or skn.".into()),
    };
    if expiry <= now() {
        return Err("The shared access signature has expired.".into());
    }
    let resource = percent_decode(&raw_resource);
    let (token_namespace, raw_token_path) = split_uri(engine, &resource).ok_or("The token's resource is not a Service Bus address.")?;
    let token_path = entity_of(&raw_token_path);
    if token_namespace != namespace {
        return Err("The token was issued for another namespace.".into());
    }
    let covered = token_path.is_empty()
        || audience_path.eq_ignore_ascii_case(&token_path)
        || audience_path.to_ascii_lowercase().starts_with(&format!("{}/", token_path.to_ascii_lowercase()));
    if !covered {
        return Err("The token was issued for another entity.".into());
    }

    // The signature is over the resource as written in the token, and the expiry.
    let to_sign = format!("{raw_resource}\n{expiry}");
    let rights = matching_rule(engine, &namespace, &token_path, &key_name, &to_sign, &signature)
        .ok_or("The shared access signature is not valid for any key of the named rule.")?;
    let has = |right: &str| rights.iter().any(|r| r == right || r == "Manage");
    Ok(Grant {
        namespace,
        path: token_path.to_ascii_lowercase(),
        send: has("Send"),
        listen: has("Listen"),
        expires: expiry,
    })
}

/// The rights of the rule called `key_name` (on the namespace or on the entity
/// the token is for) whose key produced `signature`.
fn matching_rule(engine: &Engine, namespace: &str, path: &str, key_name: &str, to_sign: &str, signature: &str) -> Option<Vec<String>> {
    let candidates: Vec<(String, Vec<String>)> = {
        let state = engine.state::<ArmState>();
        let state = state.lock();
        let ns = state
            .resources()
            .find(|r| r.resource_type.eq_ignore_ascii_case("Microsoft.ServiceBus/namespaces") && r.name.eq_ignore_ascii_case(namespace))?
            .clone();
        let mut ids = vec![format!("{}/AuthorizationRules/{key_name}", ns.id)];
        let entity: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        match entity.as_slice() {
            [name] => {
                ids.push(format!("{}/queues/{name}/authorizationRules/{key_name}", ns.id));
                ids.push(format!("{}/topics/{name}/authorizationRules/{key_name}", ns.id));
            }
            [topic, ..] => ids.push(format!("{}/topics/{topic}/authorizationRules/{key_name}", ns.id)),
            [] => {}
        }
        ids.iter()
            .filter_map(|id| state.resource(id))
            .map(|rule| {
                let rights = rule
                    .properties
                    .get("rights")
                    .and_then(|r| r.as_array())
                    .map(|r| r.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                (rule.id.clone(), rights)
            })
            .collect()
    };
    let expected = base64::engine::general_purpose::STANDARD.decode(signature).ok()?;
    for (rule_id, rights) in candidates {
        let keys = rule_keys(engine, &rule_id);
        for key in keys {
            // Service Bus signs with the key's text, not its decoded bytes.
            let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).ok()?;
            mac.update(to_sign.as_bytes());
            if mac.verify_slice(&expected).is_ok() {
                return Some(rights);
            }
        }
    }
    None
}

/// The rule's keys, minted on first use exactly as `listKeys` would.
fn rule_keys(engine: &Engine, rule_id: &str) -> [String; 2] {
    let state = engine.state::<ServiceBusState>();
    let mut state = state.lock();
    state
        .keys
        .entry(rule_id.to_ascii_lowercase())
        .or_insert_with(|| [crate::namespaces::new_key(), crate::namespaces::new_key()])
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris() {
        assert_eq!(
            split_host("sb://orders.servicebus.azure.localhost:5672/jobs"),
            Some(("orders.servicebus.azure.localhost".into(), "jobs".into()))
        );
        assert_eq!(
            split_host("amqps://Orders.servicebus.azure.localhost/events/Subscriptions/audit/"),
            Some(("orders.servicebus.azure.localhost".into(), "events/Subscriptions/audit".into()))
        );
        assert_eq!(split_host("sb://localhost"), Some(("localhost".into(), String::new())));
    }

    #[test]
    fn management_nodes_belong_to_their_entity() {
        assert_eq!(entity_of("t/Subscriptions/s/$management"), "t/Subscriptions/s");
        assert_eq!(entity_of("jobs/$Management"), "jobs");
        assert_eq!(entity_of("jobs"), "jobs");
    }

    #[test]
    fn grants_cover_paths_below_them() {
        let grant = Grant { namespace: "orders".into(), path: "events".into(), send: true, listen: false, expires: u64::MAX };
        assert!(grant.allows("orders", "Events/Subscriptions/audit", Right::Send));
        assert!(!grant.allows("orders", "eventsx", Right::Send));
        assert!(!grant.allows("orders", "events", Right::Listen));
        assert!(!grant.allows("other", "events", Right::Send));
    }
}
