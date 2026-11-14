//! Token issuing and verification.
//!
//! The emulator plays Entra ID: it signs RS256 access tokens with its own key and
//! every service verifies the signature, expiry and audience of the bearer token it
//! is handed. The key is generated once per process and never leaves it.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use http::{header, HeaderMap};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rsa::pkcs1::EncodeRsaPrivateKey;
use rsa::traits::PublicKeyParts;
use rsa::RsaPrivateKey;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::Config;

/// Lifetime of an issued access token, in seconds.
pub const TOKEN_LIFETIME_SECS: u64 = 3600;
const KEY_ID: &str = "azure-emulator-key-1";

/// The client id the emulator uses for its own startup provisioning; it is
/// allowed everything when role assignments are enforced.
pub const PROVISIONING_CLIENT: &str = "azure-emulator-config";

struct SigningKeys {
    encoding: EncodingKey,
    decoding: DecodingKey,
    modulus: String,
    exponent: String,
}

fn keys() -> &'static SigningKeys {
    static KEYS: OnceLock<SigningKeys> = OnceLock::new();
    KEYS.get_or_init(|| {
        let private = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048)
            .expect("generating the token signing key");
        let pem = private
            .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
            .expect("encoding the token signing key");
        let modulus = URL_SAFE_NO_PAD.encode(private.n().to_bytes_be());
        let exponent = URL_SAFE_NO_PAD.encode(private.e().to_bytes_be());
        SigningKeys {
            encoding: EncodingKey::from_rsa_pem(pem.as_bytes()).expect("loading the signing key"),
            decoding: DecodingKey::from_rsa_components(&modulus, &exponent)
                .expect("loading the verification key"),
            modulus,
            exponent,
        }
    })
}

/// Claims of an emulator-issued access token.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claims {
    pub aud: String,
    pub iss: String,
    pub iat: u64,
    pub nbf: u64,
    pub exp: u64,
    pub tid: String,
    pub oid: String,
    pub sub: String,
    pub appid: String,
    pub ver: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Signs an access token for `audience` on behalf of the application `client_id`.
pub fn issue_token(config: &Config, audience: &str, client_id: &str) -> String {
    issue_token_for(config, audience, client_id, client_id)
}

/// A token for `client_id` whose principal (`oid`, `sub`) is `object_id`, as a
/// managed identity's token names the identity's principal id.
pub fn issue_token_for(config: &Config, audience: &str, client_id: &str, object_id: &str) -> String {
    let iat = now();
    let claims = Claims {
        aud: audience.to_string(),
        iss: config.issuer(),
        iat,
        nbf: iat,
        exp: iat + TOKEN_LIFETIME_SECS,
        tid: config.tenant_id.clone(),
        // A stable object id per principal, so role assignments can name it.
        oid: object_id.to_string(),
        sub: object_id.to_string(),
        appid: client_id.to_string(),
        ver: "1.0".into(),
    };
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(KEY_ID.into());
    jsonwebtoken::encode(&header, &claims, &keys().encoding).expect("signing an access token")
}

/// The JSON Web Key Set the token service publishes.
pub fn jwks() -> Value {
    let keys = keys();
    json!({"keys": [{
        "kty": "RSA", "use": "sig", "alg": "RS256", "kid": KEY_ID,
        "n": keys.modulus, "e": keys.exponent,
    }]})
}

/// Why a request's bearer token was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum AuthFailure {
    /// No `Authorization: Bearer` header.
    Missing,
    /// Present but not a valid, unexpired emulator-issued token.
    Invalid,
    /// Valid token, but issued for a different resource.
    WrongAudience,
}

fn normalize_audience(audience: &str) -> String {
    audience.trim_end_matches('/').to_ascii_lowercase()
}

/// The bearer token of a request, if it carries one.
pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

/// Verifies a token and that it was issued for one of `audiences` (compared
/// without regard to case or a trailing slash).
pub fn verify_token(token: &str, audiences: &[String]) -> Result<Claims, AuthFailure> {
    let mut validation = Validation::new(Algorithm::RS256);
    validation.validate_aud = false;
    validation.leeway = 0;
    let claims = jsonwebtoken::decode::<Claims>(token, &keys().decoding, &validation)
        .map_err(|_| AuthFailure::Invalid)?
        .claims;
    let audience = normalize_audience(&claims.aud);
    if audiences.iter().any(|a| normalize_audience(a) == audience) {
        Ok(claims)
    } else {
        Err(AuthFailure::WrongAudience)
    }
}

/// Verifies the request's bearer token and that it was issued for one of
/// `audiences` (compared without regard to case or a trailing slash).
pub fn authorize(headers: &HeaderMap, audiences: &[String]) -> Result<Claims, AuthFailure> {
    let token = bearer_token(headers).ok_or(AuthFailure::Missing)?;
    verify_token(token, audiences)
}
