//! Key material, and the cryptographic operations the keys data plane
//! performs with it: RSA (encrypt, decrypt, wrap, unwrap, sign, verify) and
//! elliptic curves P-256, P-256K, P-384 and P-521 (sign, verify).
//!
//! Keys are exchanged as JSON Web Keys, with big-endian integers in
//! unpadded base64url, as Key Vault does.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::rngs::OsRng;
use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use rsa::{BigUint, Oaep, Pkcs1v15Encrypt, Pkcs1v15Sign, Pss, RsaPrivateKey, RsaPublicKey};
use serde_json::{json, Map, Value};
use sha2::{Sha256, Sha384, Sha512};
use signature::hazmat::{PrehashSigner, PrehashVerifier};

/// A key's private material.
#[derive(Clone)]
pub(crate) enum Material {
    Rsa(Box<RsaPrivateKey>),
    /// The private scalar, big-endian, as long as the curve's field.
    Ec { curve: Curve, d: Vec<u8> },
    /// A symmetric key (managed HSMs only; no operations are served for it).
    Oct(Vec<u8>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Curve {
    P256,
    P256K,
    P384,
    P521,
}

impl Curve {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "P-256" => Self::P256,
            "P-256K" | "SECP256K1" => Self::P256K,
            "P-384" => Self::P384,
            "P-521" => Self::P521,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::P256 => "P-256",
            Self::P256K => "P-256K",
            Self::P384 => "P-384",
            Self::P521 => "P-521",
        }
    }

    /// The bytes in a coordinate or private scalar.
    fn field_len(self) -> usize {
        match self {
            Self::P256 | Self::P256K => 32,
            Self::P384 => 48,
            Self::P521 => 66,
        }
    }

    /// The one signature algorithm a key on this curve signs with.
    fn algorithm(self) -> &'static str {
        match self {
            Self::P256 => "ES256",
            Self::P256K => "ES256K",
            Self::P384 => "ES384",
            Self::P521 => "ES512",
        }
    }
}

/// Runs `$body` with `$m` bound to the crate of `$curve`.
macro_rules! with_curve {
    ($curve:expr, $m:ident => $body:expr) => {
        match $curve {
            Curve::P256 => {
                use p256 as $m;
                $body
            }
            Curve::P256K => {
                use k256 as $m;
                $body
            }
            Curve::P384 => {
                use p384 as $m;
                $body
            }
            Curve::P521 => {
                use p521 as $m;
                $body
            }
        }
    };
}

fn b64(bytes: &[u8]) -> Value {
    json!(URL_SAFE_NO_PAD.encode(bytes))
}

/// Decodes base64url, padded or not, as clients send either.
pub(crate) fn unb64(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text.trim_end_matches('=')).ok()
}

fn big(value: &BigUint) -> Value {
    b64(&value.to_bytes_be())
}

fn left_pad(bytes: &[u8], len: usize) -> Option<Vec<u8>> {
    let bytes = match bytes.iter().position(|b| *b != 0) {
        Some(start) => &bytes[start..],
        None => &[][..],
    };
    (bytes.len() <= len).then(|| {
        let mut out = vec![0; len - bytes.len()];
        out.extend_from_slice(bytes);
        out
    })
}

impl Material {
    /// A new RSA key of `bits`, with `exponent` (65537 if not given).
    pub(crate) fn new_rsa(bits: usize, exponent: Option<u64>) -> Result<Self, String> {
        let exponent = BigUint::from(exponent.unwrap_or(65537));
        RsaPrivateKey::new_with_exp(&mut OsRng, bits, &exponent).map(|k| Self::Rsa(Box::new(k))).map_err(|e| e.to_string())
    }

    pub(crate) fn new_ec(curve: Curve) -> Self {
        let d = with_curve!(curve, m => m::SecretKey::random(&mut OsRng).to_bytes().to_vec());
        Self::Ec { curve, d }
    }

    pub(crate) fn new_oct(bits: usize) -> Self {
        use rand::RngCore;
        let mut bytes = vec![0; bits / 8];
        OsRng.fill_bytes(&mut bytes);
        Self::Oct(bytes)
    }

    /// The key's public JSON Web Key members (`n` and `e`, or `crv`, `x` and `y`).
    pub(crate) fn public_jwk(&self) -> Map<String, Value> {
        let mut out = Map::new();
        match self {
            Self::Rsa(key) => {
                out.insert("n".into(), big(key.n()));
                out.insert("e".into(), big(key.e()));
            }
            Self::Ec { curve, d } => {
                let point = with_curve!(*curve, m => {
                    let key = m::ecdsa::SigningKey::from_slice(d).expect("stored scalars are valid");
                    let point = m::ecdsa::VerifyingKey::from(&key).to_encoded_point(false);
                    (point.x().map(|x| x.to_vec()), point.y().map(|y| y.to_vec()))
                });
                out.insert("crv".into(), json!(curve.name()));
                out.insert("x".into(), b64(&point.0.unwrap_or_default()));
                out.insert("y".into(), b64(&point.1.unwrap_or_default()));
            }
            Self::Oct(_) => {}
        }
        out
    }

    /// Every member of the key's JSON Web Key, private ones included, for a backup.
    pub(crate) fn private_jwk(&self) -> Map<String, Value> {
        let mut out = self.public_jwk();
        match self {
            Self::Rsa(key) => {
                out.insert("d".into(), big(key.d()));
                if let [p, q] = key.primes() {
                    out.insert("p".into(), big(p));
                    out.insert("q".into(), big(q));
                }
            }
            Self::Ec { d, .. } => {
                out.insert("d".into(), b64(d));
            }
            Self::Oct(k) => {
                out.insert("k".into(), b64(k));
            }
        }
        out
    }

    /// The material of an imported JSON Web Key; it must hold the private key.
    pub(crate) fn from_jwk(jwk: &Value) -> Result<Self, String> {
        let field = |name: &str| -> Result<Vec<u8>, String> {
            jwk.get(name).and_then(Value::as_str).and_then(unb64).ok_or_else(|| format!("The key's '{name}' is missing or not base64url."))
        };
        let kty = jwk.get("kty").and_then(Value::as_str).unwrap_or("");
        match kty.trim_end_matches("-HSM") {
            "RSA" => {
                let int = |name: &str| field(name).map(|b| BigUint::from_bytes_be(&b));
                let mut primes = Vec::new();
                if jwk.get("p").is_some() {
                    primes = vec![int("p")?, int("q")?];
                }
                let mut key = RsaPrivateKey::from_components(int("n")?, int("e")?, int("d")?, primes).map_err(|e| e.to_string())?;
                key.validate().map_err(|e| e.to_string())?;
                key.precompute().map_err(|e| e.to_string())?;
                Ok(Self::Rsa(Box::new(key)))
            }
            "EC" => {
                let curve = jwk.get("crv").and_then(Value::as_str).and_then(Curve::from_name).ok_or("The key's curve is missing or not supported.")?;
                let d = left_pad(&field("d")?, curve.field_len()).ok_or("The key's 'd' is too long for its curve.")?;
                let valid = with_curve!(curve, m => m::ecdsa::SigningKey::from_slice(&d).is_ok());
                if !valid {
                    return Err("The key's 'd' is not a valid private key.".into());
                }
                let material = Self::Ec { curve, d };
                // The public point, when given, must be the private key's.
                let public = material.public_jwk();
                for coordinate in ["x", "y"] {
                    if let Some(given) = jwk.get(coordinate).and_then(Value::as_str).and_then(unb64) {
                        let ours = public.get(coordinate).and_then(Value::as_str).and_then(unb64).unwrap_or_default();
                        if left_pad(&given, curve.field_len()) != Some(ours) {
                            return Err("The key's public point does not match its private key.".into());
                        }
                    }
                }
                Ok(material)
            }
            "oct" => Ok(Self::Oct(field("k")?)),
            _ => Err(format!("The key type '{kty}' is not supported.")),
        }
    }

    /// Encrypt or wrap with `RSA1_5`, `RSA-OAEP` or `RSA-OAEP-256`.
    pub(crate) fn encrypt(&self, alg: &str, data: &[u8]) -> Result<Vec<u8>, String> {
        let Self::Rsa(key) = self else { return Err(unsupported(alg)) };
        let public = RsaPublicKey::from(key.as_ref());
        match alg {
            "RSA1_5" => public.encrypt(&mut OsRng, Pkcs1v15Encrypt, data),
            "RSA-OAEP" => public.encrypt(&mut OsRng, Oaep::new::<sha1::Sha1>(), data),
            "RSA-OAEP-256" => public.encrypt(&mut OsRng, Oaep::new::<Sha256>(), data),
            _ => return Err(unsupported(alg)),
        }
        .map_err(|e| e.to_string())
    }

    pub(crate) fn decrypt(&self, alg: &str, data: &[u8]) -> Result<Vec<u8>, String> {
        let Self::Rsa(key) = self else { return Err(unsupported(alg)) };
        match alg {
            "RSA1_5" => key.decrypt(Pkcs1v15Encrypt, data),
            "RSA-OAEP" => key.decrypt(Oaep::new::<sha1::Sha1>(), data),
            "RSA-OAEP-256" => key.decrypt(Oaep::new::<Sha256>(), data),
            _ => return Err(unsupported(alg)),
        }
        .map_err(|_| "The ciphertext could not be decrypted with this key.".to_string())
    }

    /// Signs a digest: `RS*` and `PS*` with RSA keys, `ES*` with EC keys.
    pub(crate) fn sign(&self, alg: &str, digest: &[u8]) -> Result<Vec<u8>, String> {
        check_digest(alg, digest)?;
        match self {
            Self::Rsa(key) => match alg {
                "RS256" => key.sign(Pkcs1v15Sign::new::<Sha256>(), digest),
                "RS384" => key.sign(Pkcs1v15Sign::new::<Sha384>(), digest),
                "RS512" => key.sign(Pkcs1v15Sign::new::<Sha512>(), digest),
                "PS256" => key.sign_with_rng(&mut OsRng, Pss::new::<Sha256>(), digest),
                "PS384" => key.sign_with_rng(&mut OsRng, Pss::new::<Sha384>(), digest),
                "PS512" => key.sign_with_rng(&mut OsRng, Pss::new::<Sha512>(), digest),
                _ => return Err(unsupported(alg)),
            }
            .map_err(|e| e.to_string()),
            Self::Ec { curve, d } => {
                if alg != curve.algorithm() {
                    return Err(unsupported(alg));
                }
                with_curve!(*curve, m => {
                    let key = m::ecdsa::SigningKey::from_slice(d).map_err(|e| e.to_string())?;
                    let signature: m::ecdsa::Signature = key.sign_prehash(digest).map_err(|e| e.to_string())?;
                    Ok(signature.to_bytes().to_vec())
                })
            }
            Self::Oct(_) => Err(unsupported(alg)),
        }
    }

    pub(crate) fn verify(&self, alg: &str, digest: &[u8], signature: &[u8]) -> Result<bool, String> {
        check_digest(alg, digest)?;
        match self {
            Self::Rsa(key) => {
                let public = RsaPublicKey::from(key.as_ref());
                let result = match alg {
                    "RS256" => public.verify(Pkcs1v15Sign::new::<Sha256>(), digest, signature),
                    "RS384" => public.verify(Pkcs1v15Sign::new::<Sha384>(), digest, signature),
                    "RS512" => public.verify(Pkcs1v15Sign::new::<Sha512>(), digest, signature),
                    "PS256" => public.verify(Pss::new::<Sha256>(), digest, signature),
                    "PS384" => public.verify(Pss::new::<Sha384>(), digest, signature),
                    "PS512" => public.verify(Pss::new::<Sha512>(), digest, signature),
                    _ => return Err(unsupported(alg)),
                };
                Ok(result.is_ok())
            }
            Self::Ec { curve, d } => {
                if alg != curve.algorithm() {
                    return Err(unsupported(alg));
                }
                Ok(with_curve!(*curve, m => {
                    let key = m::ecdsa::SigningKey::from_slice(d).map_err(|e| e.to_string())?;
                    let verifying = m::ecdsa::VerifyingKey::from(&key);
                    match m::ecdsa::Signature::from_slice(signature) {
                        Ok(signature) => verifying.verify_prehash(digest, &signature).is_ok(),
                        Err(_) => false,
                    }
                }))
            }
            Self::Oct(_) => Err(unsupported(alg)),
        }
    }
}

fn unsupported(alg: &str) -> String {
    format!("The algorithm '{alg}' is not supported for this key.")
}

/// A signature algorithm's digest must be its hash's length.
fn check_digest(alg: &str, digest: &[u8]) -> Result<(), String> {
    let expected = match alg.get(2..) {
        Some("256") | Some("256K") => 32,
        Some("384") => 48,
        Some("512") => 64,
        _ => return Err(unsupported(alg)),
    };
    if digest.len() != expected {
        return Err(format!("The digest for '{alg}' must be {expected} bytes."));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;

    #[test]
    fn rsa_round_trips() {
        let key = Material::new_rsa(2048, None).unwrap();
        for alg in ["RSA1_5", "RSA-OAEP", "RSA-OAEP-256"] {
            let sealed = key.encrypt(alg, b"secret").unwrap();
            assert_eq!(key.decrypt(alg, &sealed).unwrap(), b"secret");
        }
        let digest = Sha256::digest(b"message");
        for alg in ["RS256", "PS256"] {
            let signature = key.sign(alg, &digest).unwrap();
            assert!(key.verify(alg, &digest, &signature).unwrap());
            assert!(!key.verify(alg, &Sha256::digest(b"other"), &signature).unwrap());
        }
        // A backup restores the same key.
        let mut jwk = key.private_jwk();
        jwk.insert("kty".into(), json!("RSA"));
        let restored = Material::from_jwk(&Value::Object(jwk)).unwrap();
        assert_eq!(restored.public_jwk(), key.public_jwk());
    }

    #[test]
    fn ec_curves_sign_and_verify() {
        for (curve, digest) in [
            (Curve::P256, Sha256::digest(b"m").to_vec()),
            (Curve::P256K, Sha256::digest(b"m").to_vec()),
            (Curve::P384, Sha384::digest(b"m").to_vec()),
            (Curve::P521, Sha512::digest(b"m").to_vec()),
        ] {
            let key = Material::new_ec(curve);
            let alg = curve.algorithm();
            let signature = key.sign(alg, &digest).unwrap();
            assert!(key.verify(alg, &digest, &signature).unwrap(), "{alg}");
            let mut jwk = key.private_jwk();
            jwk.insert("kty".into(), json!("EC"));
            let restored = Material::from_jwk(&Value::Object(jwk)).unwrap();
            assert_eq!(restored.public_jwk(), key.public_jwk());
            assert!(key.sign("ES999", &digest).is_err());
        }
    }
}
