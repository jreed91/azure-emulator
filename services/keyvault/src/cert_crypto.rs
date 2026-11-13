//! What certificates need from key material beyond the keys data plane:
//! PKCS#8 for the secret, the SubjectPublicKeyInfo to match a certificate
//! to its key, reading a private key from PEM or PKCS#12, and signing
//! certificates and requests (RSA with SHA-256, P-256 and P-384).

use p256::ecdsa::signature::hazmat::PrehashSigner;
use rsa::pkcs1::{DecodeRsaPrivateKey, EncodeRsaPublicKey};
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey};
use rsa::{Pkcs1v15Sign, RsaPrivateKey, RsaPublicKey};
use sha2::{Digest, Sha256, Sha384};

use crate::crypto::Material;

/// The private key as PKCS#8 DER.
pub(crate) fn pkcs8_der(material: &Material) -> Result<Vec<u8>, String> {
    let der = match material {
        Material::Rsa(key) => key.to_pkcs8_der(),
        Material::P256(key) => key.to_pkcs8_der(),
        Material::P384(key) => key.to_pkcs8_der(),
        Material::P521(key) => p521::SecretKey::from_slice(&key.to_bytes()).map_err(|e| e.to_string())?.to_pkcs8_der(),
        Material::K256(key) => key.to_pkcs8_der(),
    };
    der.map(|d| d.as_bytes().to_vec()).map_err(|e| e.to_string())
}

/// The public key as a DER SubjectPublicKeyInfo, as a certificate carries it.
pub(crate) fn spki_der(material: &Material) -> Result<Vec<u8>, String> {
    let der = match material {
        Material::Rsa(key) => RsaPublicKey::from(key.as_ref()).to_public_key_der(),
        Material::P256(key) => key.verifying_key().to_public_key_der(),
        Material::P384(key) => key.verifying_key().to_public_key_der(),
        Material::P521(key) => p521::SecretKey::from_slice(&key.to_bytes()).map_err(|e| e.to_string())?.public_key().to_public_key_der(),
        Material::K256(key) => key.verifying_key().to_public_key_der(),
    };
    der.map(|d| d.as_bytes().to_vec()).map_err(|e| e.to_string())
}

/// A private key in PKCS#8, PKCS#1 (`RSA PRIVATE KEY`) or SEC1 (`EC PRIVATE KEY`) DER.
pub(crate) fn from_private_der(der: &[u8]) -> Result<Material, String> {
    if let Ok(key) = RsaPrivateKey::from_pkcs8_der(der).or_else(|_| RsaPrivateKey::from_pkcs1_der(der)) {
        return Ok(Material::Rsa(Box::new(key)));
    }
    if let Ok(key) = p256::SecretKey::from_pkcs8_der(der).or_else(|_| p256::SecretKey::from_sec1_der(der)) {
        return Ok(Material::P256(key.into()));
    }
    if let Ok(key) = p384::SecretKey::from_pkcs8_der(der).or_else(|_| p384::SecretKey::from_sec1_der(der)) {
        return Ok(Material::P384(key.into()));
    }
    if let Ok(key) = p521::SecretKey::from_pkcs8_der(der).or_else(|_| p521::SecretKey::from_sec1_der(der)) {
        return p521::ecdsa::SigningKey::from_slice(&key.to_bytes()).map(Material::P521).map_err(|e| e.to_string());
    }
    if let Ok(key) = k256::SecretKey::from_pkcs8_der(der).or_else(|_| k256::SecretKey::from_sec1_der(der)) {
        return Ok(Material::K256(key.into()));
    }
    Err("The private key is not an RSA or EC key in PKCS#8, PKCS#1 or SEC1 form.".into())
}

/// Signs certificates and certificate requests with a vault key.
pub(crate) struct CertificateSigner {
    material: Material,
    public: Vec<u8>,
    algorithm: &'static rcgen::SignatureAlgorithm,
}

impl CertificateSigner {
    pub(crate) fn new(material: &Material) -> Result<Self, String> {
        let (public, algorithm): (Vec<u8>, &'static rcgen::SignatureAlgorithm) = match material {
            Material::Rsa(key) => (
                RsaPublicKey::from(key.as_ref()).to_pkcs1_der().map_err(|e| e.to_string())?.as_bytes().to_vec(),
                &rcgen::PKCS_RSA_SHA256,
            ),
            Material::P256(key) => (key.verifying_key().to_encoded_point(false).as_bytes().to_vec(), &rcgen::PKCS_ECDSA_P256_SHA256),
            Material::P384(key) => (key.verifying_key().to_encoded_point(false).as_bytes().to_vec(), &rcgen::PKCS_ECDSA_P384_SHA384),
            _ => return Err("Certificates support RSA keys and EC keys on P-256 and P-384.".into()),
        };
        Ok(Self { material: material.clone(), public, algorithm })
    }
}

impl rcgen::RemoteKeyPair for CertificateSigner {
    fn public_key(&self) -> &[u8] {
        &self.public
    }

    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        let failed = || rcgen::Error::RemoteKeyError;
        // X.509 carries ECDSA signatures DER-encoded, not as raw `r||s`.
        match &self.material {
            Material::Rsa(key) => key.sign(Pkcs1v15Sign::new::<Sha256>(), &Sha256::digest(message)).map_err(|_| failed()),
            Material::P256(key) => {
                let signature: p256::ecdsa::Signature = key.sign_prehash(&Sha256::digest(message)).map_err(|_| failed())?;
                Ok(signature.to_der().as_bytes().to_vec())
            }
            Material::P384(key) => {
                let signature: p384::ecdsa::Signature = key.sign_prehash(&Sha384::digest(message)).map_err(|_| failed())?;
                Ok(signature.to_der().as_bytes().to_vec())
            }
            _ => Err(failed()),
        }
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        self.algorithm
    }
}
