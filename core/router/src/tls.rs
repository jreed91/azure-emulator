//! The emulator's TLS identity.
//!
//! Azure SDKs refuse to send bearer tokens over plain HTTP, so the emulator serves
//! HTTPS. It keeps a local certificate authority in its home directory — trust
//! `ca.pem` once and every later run is trusted — and issues itself a fresh leaf
//! certificate for the management host and the wildcard data-plane hostnames.

use std::path::{Path, PathBuf};

use emulator_core::Config;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};

pub struct TlsIdentity {
    /// Leaf certificate followed by the CA certificate, PEM.
    pub certificate_chain_pem: String,
    pub private_key_pem: String,
    pub ca_path: PathBuf,
}

type Error = Box<dyn std::error::Error + Send + Sync>;

/// `$AZURE_EMULATOR_HOME`, or `~/.azure-emulator`.
pub fn home_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("AZURE_EMULATOR_HOME") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".azure-emulator")
}

fn load_or_create_ca(dir: &Path) -> Result<(rcgen::Certificate, KeyPair, String), Error> {
    let cert_path = dir.join("ca.pem");
    let key_path = dir.join("ca.key");
    if cert_path.exists() && key_path.exists() {
        let pem = std::fs::read_to_string(&cert_path)?;
        let key = KeyPair::from_pem(&std::fs::read_to_string(&key_path)?)?;
        // Rebuilt from the stored parameters and key: leaves it signs chain to the
        // certificate on disk, which has the same subject and public key.
        let certificate = CertificateParams::from_ca_cert_pem(&pem)?.self_signed(&key)?;
        return Ok((certificate, key, pem));
    }
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, "Azure Emulator Local CA");
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let key = KeyPair::generate()?;
    let certificate = params.self_signed(&key)?;
    let pem = certificate.pem();
    std::fs::create_dir_all(dir)?;
    std::fs::write(&cert_path, &pem)?;
    std::fs::write(&key_path, key.serialize_pem())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok((certificate, key, pem))
}

/// Issues the certificate this emulator serves, creating the local CA if needed.
pub fn identity(config: &Config, bind_address: &str) -> Result<TlsIdentity, Error> {
    let dir = home_dir();
    let (ca, ca_key, ca_pem) = load_or_create_ca(&dir)?;
    let mut names = vec![
        config.host.clone(),
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        format!("*{}", config.blob_host_suffix()),
        format!("*{}", config.vault_host_suffix()),
        format!("*{}", config.queue_host_suffix()),
        format!("*{}", config.table_host_suffix()),
        format!("*.servicebus.{}", config.dns_suffix),
        format!("*.documents.{}", config.dns_suffix),
    ];
    if bind_address.parse::<std::net::IpAddr>().is_ok_and(|ip| !ip.is_unspecified()) {
        names.push(bind_address.to_string());
    }
    names.sort();
    names.dedup();
    let mut params = CertificateParams::new(names)?;
    params.distinguished_name.push(DnType::CommonName, "Azure Emulator");
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    // Strict verifiers (Python 3.13 and later) reject a leaf without this extension.
    params.use_authority_key_identifier_extension = true;
    let key = KeyPair::generate()?;
    let leaf = params.signed_by(&key, &ca, &ca_key)?;
    Ok(TlsIdentity {
        certificate_chain_pem: format!("{}{}", leaf.pem(), ca_pem),
        private_key_pem: key.serialize_pem(),
        ca_path: dir.join("ca.pem"),
    })
}

/// A TLS acceptor serving `identity`, for the AMQP-over-TLS listener.
pub fn acceptor(identity: &TlsIdentity) -> Result<tokio_rustls::TlsAcceptor, Box<dyn std::error::Error + Send + Sync>> {
    let chain = rustls_pemfile::certs(&mut identity.certificate_chain_pem.as_bytes()).collect::<Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut identity.private_key_pem.as_bytes())?.ok_or("the private key PEM holds no key")?;
    let config = tokio_rustls::rustls::ServerConfig::builder().with_no_client_auth().with_single_cert(chain, key)?;
    Ok(tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config)))
}
