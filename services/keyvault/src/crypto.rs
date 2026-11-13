//! Key material and the cryptographic operations Key Vault performs with it.
//!
//! Signatures are over a digest the client computed, as Key Vault's `sign` and
//! `verify` are; ECDSA signatures are `r || s`, each as long as the curve's field.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rsa::traits::PublicKeyParts;
use rsa::{BigUint, Oaep, Pkcs1v15Encrypt, Pkcs1v15Sign, Pss, RsaPrivateKey, RsaPublicKey};
use serde_json::{json, Map, Value};
use sha2::{Sha256, Sha384, Sha512};

/// A key's private material.
/// A snapshot keeps key material as its private JSON Web Key.
impl serde::Serialize for Material {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.private_jwk().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for Material {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let jwk = Value::deserialize(deserializer)?;
        Material::from_jwk(&jwk).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone)]
pub(crate) enum Material {
    Rsa(Box<RsaPrivateKey>),
    P256(p256::ecdsa::SigningKey),
    P384(p384::ecdsa::SigningKey),
    P521(p521::ecdsa::SigningKey),
    K256(k256::ecdsa::SigningKey),
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decodes base64url, with or without padding (and tolerating standard base64).
pub(crate) fn unb64(text: &str) -> Result<Vec<u8>, String> {
    let trimmed = text.trim_end_matches('=');
    URL_SAFE_NO_PAD
        .decode(trimmed)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(trimmed))
        .map_err(|_| "The value is not valid base64url.".to_string())
}

pub(crate) fn encode(bytes: &[u8]) -> String {
    b64(bytes)
}

impl Material {
    /// Generates a key: RSA of `size` bits, or EC on `curve`.
    pub(crate) fn generate(kty: &str, size: Option<i64>, curve: Option<&str>, public_exponent: Option<i64>) -> Result<Self, String> {
        let mut rng = rsa::rand_core::OsRng;
        match kty.trim_end_matches("-HSM") {
            "RSA" => {
                let bits = size.unwrap_or(2048) as usize;
                let exponent = BigUint::from(public_exponent.unwrap_or(65537) as u64);
                let key = RsaPrivateKey::new_with_exp(&mut rng, bits, &exponent).map_err(|e| format!("RSA key generation failed: {e}"))?;
                Ok(Material::Rsa(Box::new(key)))
            }
            "EC" => Ok(match curve.unwrap_or("P-256") {
                "P-256" => Material::P256(p256::ecdsa::SigningKey::random(&mut rng)),
                "P-384" => Material::P384(p384::ecdsa::SigningKey::random(&mut rng)),
                "P-521" => Material::P521(p521::ecdsa::SigningKey::random(&mut rng)),
                "P-256K" => Material::K256(k256::ecdsa::SigningKey::random(&mut rng)),
                other => return Err(format!("The curve '{other}' is not supported.")),
            }),
            other => Err(format!("Keys of type '{other}' have no material in the emulator.")),
        }
    }

    /// Reads a private key from a JSON Web Key, as Import Key takes it.
    pub(crate) fn from_jwk(jwk: &Value) -> Result<Self, String> {
        let field = |name: &str| -> Result<Vec<u8>, String> {
            let text = jwk.get(name).and_then(Value::as_str).ok_or_else(|| format!("The key's '{name}' is required."))?;
            unb64(text)
        };
        let kty = jwk.get("kty").and_then(Value::as_str).unwrap_or("");
        match kty.trim_end_matches("-HSM") {
            "RSA" => {
                let big = |name: &str| field(name).map(|b| BigUint::from_bytes_be(&b));
                let key = RsaPrivateKey::from_components(big("n")?, big("e")?, big("d")?, vec![big("p")?, big("q")?])
                    .map_err(|e| format!("The RSA key is not valid: {e}"))?;
                key.validate().map_err(|e| format!("The RSA key is not valid: {e}"))?;
                Ok(Material::Rsa(Box::new(key)))
            }
            "EC" => {
                let d = field("d")?;
                let bad = |_| "The EC private key is not valid for its curve.".to_string();
                let crv = jwk.get("crv").and_then(Value::as_str).unwrap_or("");
                Ok(match crv {
                    "P-256" => Material::P256(p256::ecdsa::SigningKey::from_slice(&d).map_err(bad)?),
                    "P-384" => Material::P384(p384::ecdsa::SigningKey::from_slice(&d).map_err(bad)?),
                    "P-521" => Material::P521(p521::ecdsa::SigningKey::from_slice(&d).map_err(bad)?),
                    "P-256K" => Material::K256(k256::ecdsa::SigningKey::from_slice(&d).map_err(bad)?),
                    other => return Err(format!("The curve '{other}' is not supported.")),
                })
            }
            other => Err(format!("Importing keys of type '{other}' is not supported.")),
        }
    }

    /// `RSA` or `EC`.
    pub(crate) fn family(&self) -> &'static str {
        match self {
            Material::Rsa(_) => "RSA",
            _ => "EC",
        }
    }

    pub(crate) fn key_size(&self) -> Option<i64> {
        match self {
            Material::Rsa(key) => Some(key.size() as i64 * 8),
            _ => None,
        }
    }

    pub(crate) fn curve(&self) -> Option<&'static str> {
        match self {
            Material::Rsa(_) => None,
            Material::P256(_) => Some("P-256"),
            Material::P384(_) => Some("P-384"),
            Material::P521(_) => Some("P-521"),
            Material::K256(_) => Some("P-256K"),
        }
    }

    /// The public half as JSON Web Key members (`n`, `e`, or `crv`, `x`, `y`).
    pub(crate) fn public_jwk(&self) -> Map<String, Value> {
        let mut out = Map::new();
        let point = |encoded: &[u8], size: usize| -> (String, String) {
            // Uncompressed SEC1: 0x04 || x || y.
            (b64(&encoded[1..1 + size]), b64(&encoded[1 + size..]))
        };
        let (x, y) = match self {
            Material::Rsa(key) => {
                out.insert("n".into(), json!(b64(&key.n().to_bytes_be())));
                out.insert("e".into(), json!(b64(&key.e().to_bytes_be())));
                return out;
            }
            Material::P256(key) => point(key.verifying_key().to_encoded_point(false).as_bytes(), 32),
            Material::P384(key) => point(key.verifying_key().to_encoded_point(false).as_bytes(), 48),
            Material::P521(key) => {
                let verifying = p521::ecdsa::VerifyingKey::from(key);
                point(verifying.to_encoded_point(false).as_bytes(), 66)
            }
            Material::K256(key) => point(key.verifying_key().to_encoded_point(false).as_bytes(), 32),
        };
        out.insert("crv".into(), json!(self.curve()));
        out.insert("x".into(), json!(x));
        out.insert("y".into(), json!(y));
        out
    }

    /// Every JSON Web Key member, private ones included, as a backup keeps them.
    pub(crate) fn private_jwk(&self) -> Map<String, Value> {
        use rsa::traits::PrivateKeyParts;
        let mut out = self.public_jwk();
        out.insert("kty".into(), json!(self.family()));
        let d = match self {
            Material::Rsa(key) => {
                if let [p, q] = key.primes() {
                    out.insert("p".into(), json!(b64(&p.to_bytes_be())));
                    out.insert("q".into(), json!(b64(&q.to_bytes_be())));
                }
                key.d().to_bytes_be()
            }
            Material::P256(key) => key.to_bytes().to_vec(),
            Material::P384(key) => key.to_bytes().to_vec(),
            Material::P521(key) => key.to_bytes().to_vec(),
            Material::K256(key) => key.to_bytes().to_vec(),
        };
        out.insert("d".into(), json!(b64(&d)));
        out
    }

    fn rsa(&self, operation: &str) -> Result<&RsaPrivateKey, String> {
        match self {
            Material::Rsa(key) => Ok(key),
            _ => Err(format!("The {operation} operation needs an RSA key.")),
        }
    }

    /// Encrypt or wrapKey.
    pub(crate) fn encrypt(&self, alg: &str, data: &[u8]) -> Result<Vec<u8>, String> {
        let public = RsaPublicKey::from(self.rsa("encrypt")?);
        let mut rng = rsa::rand_core::OsRng;
        let result = match alg {
            "RSA1_5" => public.encrypt(&mut rng, Pkcs1v15Encrypt, data),
            "RSA-OAEP" => public.encrypt(&mut rng, Oaep::new::<sha1::Sha1>(), data),
            "RSA-OAEP-256" => public.encrypt(&mut rng, Oaep::new::<Sha256>(), data),
            other => return Err(format!("The algorithm '{other}' is not supported for encryption.")),
        };
        result.map_err(|e| format!("Encryption failed: {e}"))
    }

    /// Decrypt or unwrapKey.
    pub(crate) fn decrypt(&self, alg: &str, data: &[u8]) -> Result<Vec<u8>, String> {
        let key = self.rsa("decrypt")?;
        let result = match alg {
            "RSA1_5" => key.decrypt(Pkcs1v15Encrypt, data),
            "RSA-OAEP" => key.decrypt(Oaep::new::<sha1::Sha1>(), data),
            "RSA-OAEP-256" => key.decrypt(Oaep::new::<Sha256>(), data),
            other => return Err(format!("The algorithm '{other}' is not supported for decryption.")),
        };
        result.map_err(|_| "The ciphertext could not be decrypted with this key.".to_string())
    }

    /// Signs a digest.
    pub(crate) fn sign(&self, alg: &str, digest: &[u8]) -> Result<Vec<u8>, String> {
        use p256::ecdsa::signature::hazmat::PrehashSigner;
        check_digest(alg, digest)?;
        let mut rng = rsa::rand_core::OsRng;
        let ecdsa_error = |e: p256::ecdsa::Error| format!("Signing failed: {e}");
        match (self, alg) {
            (Material::Rsa(key), "RS256") => key.sign(Pkcs1v15Sign::new::<Sha256>(), digest),
            (Material::Rsa(key), "RS384") => key.sign(Pkcs1v15Sign::new::<Sha384>(), digest),
            (Material::Rsa(key), "RS512") => key.sign(Pkcs1v15Sign::new::<Sha512>(), digest),
            (Material::Rsa(key), "PS256") => key.sign_with_rng(&mut rng, Pss::new::<Sha256>(), digest),
            (Material::Rsa(key), "PS384") => key.sign_with_rng(&mut rng, Pss::new::<Sha384>(), digest),
            (Material::Rsa(key), "PS512") => key.sign_with_rng(&mut rng, Pss::new::<Sha512>(), digest),
            (Material::P256(key), "ES256") => {
                let signature: p256::ecdsa::Signature = key.sign_prehash(digest).map_err(ecdsa_error)?;
                return Ok(signature.to_bytes().to_vec());
            }
            (Material::P384(key), "ES384") => {
                let signature: p384::ecdsa::Signature = key.sign_prehash(digest).map_err(ecdsa_error)?;
                return Ok(signature.to_bytes().to_vec());
            }
            (Material::P521(key), "ES512") => {
                let signature: p521::ecdsa::Signature = key.sign_prehash(digest).map_err(ecdsa_error)?;
                return Ok(signature.to_bytes().to_vec());
            }
            (Material::K256(key), "ES256K") => {
                let signature: k256::ecdsa::Signature = key.sign_prehash(digest).map_err(ecdsa_error)?;
                return Ok(signature.to_bytes().to_vec());
            }
            (_, other) => return Err(format!("The algorithm '{other}' does not apply to this key.")),
        }
        .map_err(|e| format!("Signing failed: {e}"))
    }

    /// Verifies a signature over a digest.
    pub(crate) fn verify(&self, alg: &str, digest: &[u8], signature: &[u8]) -> Result<bool, String> {
        use p256::ecdsa::signature::hazmat::PrehashVerifier;
        check_digest(alg, digest)?;
        Ok(match (self, alg) {
            (Material::Rsa(key), _) => {
                let public = RsaPublicKey::from(key.as_ref());
                match alg {
                    "RS256" => public.verify(Pkcs1v15Sign::new::<Sha256>(), digest, signature).is_ok(),
                    "RS384" => public.verify(Pkcs1v15Sign::new::<Sha384>(), digest, signature).is_ok(),
                    "RS512" => public.verify(Pkcs1v15Sign::new::<Sha512>(), digest, signature).is_ok(),
                    "PS256" => public.verify(Pss::new::<Sha256>(), digest, signature).is_ok(),
                    "PS384" => public.verify(Pss::new::<Sha384>(), digest, signature).is_ok(),
                    "PS512" => public.verify(Pss::new::<Sha512>(), digest, signature).is_ok(),
                    other => return Err(format!("The algorithm '{other}' does not apply to this key.")),
                }
            }
            (Material::P256(key), "ES256") => p256::ecdsa::Signature::from_slice(signature)
                .is_ok_and(|s| key.verifying_key().verify_prehash(digest, &s).is_ok()),
            (Material::P384(key), "ES384") => p384::ecdsa::Signature::from_slice(signature)
                .is_ok_and(|s| key.verifying_key().verify_prehash(digest, &s).is_ok()),
            (Material::P521(key), "ES512") => p521::ecdsa::Signature::from_slice(signature)
                .is_ok_and(|s| p521::ecdsa::VerifyingKey::from(key).verify_prehash(digest, &s).is_ok()),
            (Material::K256(key), "ES256K") => k256::ecdsa::Signature::from_slice(signature)
                .is_ok_and(|s| key.verifying_key().verify_prehash(digest, &s).is_ok()),
            (_, other) => return Err(format!("The algorithm '{other}' does not apply to this key.")),
        })
    }
}

/// A digest's length must match the algorithm's hash.
fn check_digest(alg: &str, digest: &[u8]) -> Result<(), String> {
    let expected = match alg {
        "RS256" | "PS256" | "ES256" | "ES256K" => 32,
        "RS384" | "PS384" | "ES384" => 48,
        "RS512" | "PS512" | "ES512" => 64,
        other => return Err(format!("The algorithm '{other}' is not a signature algorithm.")),
    };
    if digest.len() != expected {
        return Err(format!("Invalid digest length: {alg} needs a {expected}-byte digest, not {} bytes.", digest.len()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;

    #[test]
    fn rsa_round_trips() {
        let key = Material::generate("RSA", Some(2048), None, None).unwrap();
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
        assert!(key.sign("RS256", b"short").is_err());
        let jwk = key.public_jwk();
        assert_eq!(unb64(jwk["e"].as_str().unwrap()).unwrap(), vec![1, 0, 1]);
        assert_eq!(key.key_size(), Some(2048));
    }

    #[test]
    fn ec_curves_sign_and_verify() {
        for (curve, alg, digest) in [
            ("P-256", "ES256", Sha256::digest(b"m").to_vec()),
            ("P-384", "ES384", Sha384::digest(b"m").to_vec()),
            ("P-521", "ES512", Sha512::digest(b"m").to_vec()),
            ("P-256K", "ES256K", Sha256::digest(b"m").to_vec()),
        ] {
            let key = Material::generate("EC", None, Some(curve), None).unwrap();
            let signature = key.sign(alg, &digest).unwrap();
            assert!(key.verify(alg, &digest, &signature).unwrap(), "{curve}");
            assert_eq!(key.public_jwk()["crv"], curve);
            assert!(key.encrypt("RSA-OAEP", b"x").is_err());
        }
    }
}
