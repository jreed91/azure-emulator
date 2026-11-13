//! Shared access signatures (SAS) for the Blob service: building the string a
//! token signs, and deciding whether a token authorizes a request.
//!
//! Account SAS and service SAS (for a container or a blob) signed with an account
//! key are supported. Tokens that name a stored access policy (`si`) or are signed
//! with a user delegation key (`skoid`) are not.

use std::collections::BTreeMap;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// What kind of resource a request acts on, in account-SAS terms.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    Service,
    Container,
    Object,
}

/// What a request needs from a token: the resource level it acts on and the
/// permission letters, any one of which allows it.
#[derive(Clone, Copy)]
pub(crate) struct Needed {
    pub level: Level,
    pub permissions: &'static str,
    /// Whether a service SAS for the container may perform this operation. Of the
    /// container-level operations, that is only listing blobs.
    pub service_sas_on_container: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SasFailure {
    /// Bad signature, or outside the token's validity period.
    Authentication,
    Permission,
    ResourceType,
    Service,
    /// A kind of token the emulator does not verify.
    Unsupported,
}

/// `sv` values compare correctly as strings: they are ISO dates.
const SIGNS_RESOURCE_TYPE_FROM: &str = "2018-11-09";
const SIGNS_ENCRYPTION_SCOPE_FROM: &str = "2020-12-06";

/// The fields of a token, by query parameter name; absent ones are empty.
pub(crate) struct Fields<'a>(pub &'a BTreeMap<String, String>);

impl Fields<'_> {
    pub(crate) fn get(&self, name: &str) -> &str {
        self.0.get(name).map(String::as_str).unwrap_or("")
    }
}

/// The string an account SAS signs.
pub(crate) fn account_string_to_sign(account: &str, fields: &Fields<'_>) -> String {
    let version = fields.get("sv");
    let mut parts = vec![
        account,
        fields.get("sp"),
        fields.get("ss"),
        fields.get("srt"),
        fields.get("st"),
        fields.get("se"),
        fields.get("sip"),
        fields.get("spr"),
        version,
    ];
    if version >= SIGNS_ENCRYPTION_SCOPE_FROM {
        parts.push(fields.get("ses"));
    }
    // Every field, the last included, is followed by a newline.
    parts.iter().map(|part| format!("{part}\n")).collect()
}

/// The string a service SAS signs for `canonical_resource`
/// (`/blob/{account}/{container}[/{blob}]`).
pub(crate) fn service_string_to_sign(canonical_resource: &str, fields: &Fields<'_>) -> String {
    let version = fields.get("sv");
    let mut parts = vec![
        fields.get("sp"),
        fields.get("st"),
        fields.get("se"),
        canonical_resource,
        fields.get("si"),
        fields.get("sip"),
        fields.get("spr"),
        version,
    ];
    if version >= SIGNS_RESOURCE_TYPE_FROM {
        parts.push(fields.get("sr"));
        parts.push(fields.get("snapshot"));
    }
    if version >= SIGNS_ENCRYPTION_SCOPE_FROM {
        parts.push(fields.get("ses"));
    }
    parts.extend(["rscc", "rscd", "rsce", "rscl", "rsct"].map(|name| fields.get(name)));
    parts.join("\n")
}

/// The base64 HMAC-SHA256 of `string_to_sign` under the base64 account `key`.
pub(crate) fn sign(key: &str, string_to_sign: &str) -> Option<String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(&STANDARD.decode(key).ok()?).ok()?;
    mac.update(string_to_sign.as_bytes());
    Some(STANDARD.encode(mac.finalize().into_bytes()))
}

pub(crate) fn signature_matches(keys: &[String; 2], string_to_sign: &str, signature: &str) -> bool {
    let Ok(signature) = STANDARD.decode(signature) else {
        return false;
    };
    keys.iter().any(|key| {
        let Some(mut mac) = STANDARD
            .decode(key)
            .ok()
            .and_then(|key| Hmac::<Sha256>::new_from_slice(&key).ok())
        else {
            return false;
        };
        mac.update(string_to_sign.as_bytes());
        mac.verify_slice(&signature).is_ok()
    })
}

/// Parses the time formats a SAS accepts: a date, or a UTC date and time with
/// minutes, seconds or fractional seconds.
pub(crate) fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    if let Ok(time) = DateTime::parse_from_rfc3339(text) {
        return Some(time.with_timezone(&Utc));
    }
    if let Ok(time) = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%MZ") {
        return Some(time.and_utc());
    }
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|time| time.and_utc())
}

/// Decides whether the token in `query` (lowercased names, decoded values)
/// authorizes a request that needs `needed` on `container`/`blob` of `account`.
pub(crate) fn authorize(
    query: &BTreeMap<String, String>,
    account: &str,
    keys: &[String; 2],
    container: &str,
    blob: &str,
    needed: Needed,
) -> Result<(), SasFailure> {
    let fields = Fields(query);
    if !fields.get("skoid").is_empty() || !fields.get("si").is_empty() {
        return Err(SasFailure::Unsupported);
    }
    let is_account_sas = !fields.get("ss").is_empty() || !fields.get("srt").is_empty();
    let string_to_sign = if is_account_sas {
        account_string_to_sign(account, &fields)
    } else {
        // The signature binds the token to one container or one blob.
        let resource = match fields.get("sr") {
            "c" => format!("/blob/{account}/{container}"),
            "b" => format!("/blob/{account}/{container}/{blob}"),
            _ => return Err(SasFailure::Unsupported),
        };
        service_string_to_sign(&resource, &fields)
    };
    if fields.get("sv").is_empty() || !signature_matches(keys, &string_to_sign, fields.get("sig")) {
        return Err(SasFailure::Authentication);
    }

    let now = Utc::now();
    let expires = parse_time(fields.get("se")).ok_or(SasFailure::Authentication)?;
    let starts = match fields.get("st") {
        "" => None,
        text => Some(parse_time(text).ok_or(SasFailure::Authentication)?),
    };
    if now >= expires || starts.is_some_and(|start| now < start) {
        return Err(SasFailure::Authentication);
    }

    if is_account_sas {
        if !fields.get("ss").contains('b') {
            return Err(SasFailure::Service);
        }
        let level = match needed.level {
            Level::Service => 's',
            Level::Container => 'c',
            Level::Object => 'o',
        };
        if !fields.get("srt").contains(level) {
            return Err(SasFailure::ResourceType);
        }
    } else {
        let allowed = match (needed.level, fields.get("sr")) {
            (Level::Object, _) => true,
            (Level::Container, "c") => needed.service_sas_on_container,
            _ => false,
        };
        if !allowed {
            return Err(SasFailure::Permission);
        }
    }
    if !needed.permissions.chars().any(|permission| fields.get("sp").contains(permission)) {
        return Err(SasFailure::Permission);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn account_string_grows_with_the_version() {
        let old = fields(&[("sv", "2015-04-05"), ("sp", "r"), ("ss", "b"), ("srt", "o"), ("se", "2030-01-01")]);
        assert_eq!(account_string_to_sign("acct", &Fields(&old)), "acct\nr\nb\no\n\n2030-01-01\n\n\n2015-04-05\n");
        let new = fields(&[("sv", "2022-11-02"), ("sp", "r"), ("ss", "b"), ("srt", "o"), ("se", "2030-01-01")]);
        assert_eq!(account_string_to_sign("acct", &Fields(&new)), "acct\nr\nb\no\n\n2030-01-01\n\n\n2022-11-02\n\n");
    }

    #[test]
    fn service_string_grows_with_the_version() {
        let old = fields(&[("sv", "2015-04-05"), ("sp", "r"), ("se", "2030-01-01"), ("sr", "b")]);
        assert_eq!(
            service_string_to_sign("/blob/acct/c/b", &Fields(&old)),
            "r\n\n2030-01-01\n/blob/acct/c/b\n\n\n\n2015-04-05\n\n\n\n\n"
        );
        let new = fields(&[("sv", "2022-11-02"), ("sp", "r"), ("se", "2030-01-01"), ("sr", "b")]);
        assert_eq!(
            service_string_to_sign("/blob/acct/c/b", &Fields(&new)),
            "r\n\n2030-01-01\n/blob/acct/c/b\n\n\n\n2022-11-02\nb\n\n\n\n\n\n\n"
        );
    }

    #[test]
    fn times() {
        assert!(parse_time("2030-01-01").is_some());
        assert!(parse_time("2030-01-01T10:20Z").is_some());
        assert!(parse_time("2030-01-01T10:20:30Z").is_some());
        assert!(parse_time("2030-01-01T10:20:30.1234567Z").is_some());
        assert!(parse_time("tomorrow").is_none());
    }
}
