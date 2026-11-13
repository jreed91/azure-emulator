//! The Key Vault certificates data plane, on `{vault}.vault.<suffix>/certificates`.
//!
//! A certificate is three things under one name and version, as in Azure: the
//! X.509 certificate, its key (in the vault's keys) and a secret holding the
//! private key and certificate as PKCS#12 or PEM (in the vault's secrets).
//!
//! Implemented: create with the `Self` issuer (signed with its own key, at
//! once) or the `Unknown` issuer (a CSR to have signed elsewhere, then merge);
//! import from PKCS#12 (encrypted or not) or PEM; get (latest or by version),
//! update, list, list versions; the policy; the pending operation (get,
//! cancel, delete); soft delete, get and list deleted, recover, purge; backup
//! and restore; contacts and issuers. Keys are RSA (2048, 3072, 4096) or EC on
//! P-256 or P-384.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use emulator_core::{ArmError, Engine};
use http::{Method, StatusCode};
use serde_json::{json, Map, Value};
use sha1::Digest as _;

use crate::cert_crypto::{self, CertificateSigner};
use crate::crypto::{unb64, Material};
use crate::keys::{applicable_ops, Key, KeyVersion};
use crate::secrets::{Secret, Vault};
use crate::KeyVaultState;

const RECOVERABLE_DAYS: i64 = 90;
const BACKUP_MARKER: &str = "azure-emulator-certificate-backup-v1";
const PKCS12: &str = "application/x-pkcs12";
const PEM: &str = "application/x-pem-file";

pub(crate) struct Certificate {
    name: String,
    /// Oldest first; the last one is the current version.
    versions: Vec<CertificateVersion>,
    /// The policy in the data plane's shape, without its `id`.
    policy: Value,
    /// The last create operation, while it is pending and after it ends.
    operation: Option<Operation>,
}

#[derive(Clone)]
struct CertificateVersion {
    version: String,
    der: Vec<u8>,
    /// The certificates after the leaf, issuer first.
    chain: Vec<Vec<u8>>,
    enabled: bool,
    tags: Option<Value>,
    created: i64,
    updated: i64,
    not_before: i64,
    expires: i64,
}

struct Operation {
    /// `inProgress`, `completed` or `cancelled`.
    status: &'static str,
    issuer: String,
    /// The version the operation makes, and its key while it is pending.
    version: String,
    material: Option<Material>,
    csr: Option<Vec<u8>>,
    cancellation_requested: bool,
    request_id: String,
    enabled: bool,
    tags: Option<Value>,
}

/// A deleted certificate, with the key and secret it took with it.
pub(crate) struct DeletedCertificate {
    certificate: Certificate,
    key: Option<Key>,
    secret: Option<Secret>,
    deleted: i64,
}

pub(crate) enum Answer {
    Json(StatusCode, Value),
    Empty(StatusCode),
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn bad(message: impl Into<String>) -> ArmError {
    ArmError::bad_request("BadParameter", message)
}

fn not_found(name: &str) -> ArmError {
    ArmError::not_found("CertificateNotFound", format!("A certificate with (name/id) {name} was not found in this key vault. If you recently deleted this certificate you may be able to recover it using the correct recovery command. For help resolving this issue, please see https://go.microsoft.com/fwlink/?linkid=2125182"))
}

fn deleted_not_found(name: &str) -> ArmError {
    ArmError::not_found("CertificateNotFound", format!("Deleted Certificate not found: {name}"))
}

fn pending_not_found(name: &str) -> ArmError {
    ArmError::not_found("PendingCertificateNotFound", format!("Pending certificate not found: {name}"))
}

fn in_deleted_state(name: &str) -> ArmError {
    ArmError::conflict(
        "Conflict",
        format!("Certificate {name} is currently in a deleted but recoverable state, and its name cannot be reused; in this state, the certificate can only be recovered or purged."),
    )
}

/// What an operation needs: its data action under role-based access control,
/// and its permission (lowercased) under an access policy's `certificates`.
pub(crate) fn permission_for(method: &Method, collection: &str, rest: &[&str]) -> Option<(&'static str, &'static str)> {
    macro_rules! action {
        ($name:literal) => {
            concat!("Microsoft.KeyVault/vaults/certificates/", $name)
        };
    }
    const CAS_READ: &str = "Microsoft.KeyVault/vaults/certificatecas/read";
    const CAS_WRITE: &str = "Microsoft.KeyVault/vaults/certificatecas/write";
    const CAS_DELETE: &str = "Microsoft.KeyVault/vaults/certificatecas/delete";
    let lower: Vec<String> = rest.iter().map(|s| s.to_ascii_lowercase()).collect();
    let rest: Vec<&str> = lower.iter().map(String::as_str).collect();
    Some(match (collection, rest.as_slice(), method.as_str()) {
        ("certificates", ["contacts"], _) => ("Microsoft.KeyVault/vaults/certificatecontacts/write", "managecontacts"),
        ("certificates", ["issuers"], "GET") => (CAS_READ, "listissuers"),
        ("certificates", ["issuers", _], "GET") => (CAS_READ, "getissuers"),
        ("certificates", ["issuers", _], "DELETE") => (CAS_DELETE, "deleteissuers"),
        ("certificates", ["issuers", _], _) => (CAS_WRITE, "setissuers"),
        ("certificates", [], "GET") | ("certificates", [_, "versions"], "GET") => (action!("read"), "list"),
        ("certificates", ["restore"], "POST") => (action!("restore/action"), "restore"),
        ("certificates", [_, "create"], "POST") => (action!("create/action"), "create"),
        ("certificates", [_, "import"], "POST") => (action!("import/action"), "import"),
        ("certificates", [_, "backup"], "POST") => (action!("backup/action"), "backup"),
        ("certificates", [_] | [_, _], "GET") => (action!("read"), "get"),
        ("certificates", [_], "DELETE") => (action!("delete"), "delete"),
        ("certificates", [_, "pending", "merge"], "POST") | ("certificates", [_] | [_, _], "PATCH") | ("certificates", [_, "pending"], "DELETE") => {
            (action!("update/action"), "update")
        }
        ("deletedcertificates", [], "GET") => (action!("read"), "list"),
        ("deletedcertificates", [_], "GET") => (action!("read"), "get"),
        ("deletedcertificates", [_], "DELETE") => (action!("purge/action"), "purge"),
        ("deletedcertificates", [_, "recover"], "POST") => (action!("recover/action"), "recover"),
        _ => return None,
    })
}

/// The policy a certificate gets where the request leaves members out.
fn default_policy() -> Value {
    json!({
        "key_props": {"exportable": true, "kty": "RSA", "key_size": 2048, "reuse_key": false},
        "secret_props": {"contentType": PKCS12},
        "x509_props": {
            "ekus": ["1.3.6.1.5.5.7.3.1", "1.3.6.1.5.5.7.3.2"],
            "key_usage": ["digitalSignature", "keyEncipherment"],
            "validity_months": 12,
        },
        "lifetime_actions": [{"trigger": {"lifetime_percentage": 80}, "action": {"action_type": "AutoRenew"}}],
        "issuer": {"name": "Unknown"},
        "attributes": {"enabled": true},
    })
}

/// `base` with the members `given` sets: objects merge a level deep, the rest replace.
fn merge_policy(base: &Value, given: Option<&Value>) -> Value {
    let mut out = base.clone();
    let Some(Value::Object(given)) = given else { return out };
    for (member, value) in given {
        if member == "id" || value.is_null() {
            continue;
        }
        match (out.get_mut(member), value) {
            (Some(Value::Object(current)), Value::Object(new)) => {
                for (k, v) in new {
                    current.insert(k.clone(), v.clone());
                }
            }
            _ => out[member] = value.clone(),
        }
    }
    // An EC key has a curve and no size, and signs rather than enciphers.
    if out.pointer("/key_props/kty").and_then(Value::as_str).is_some_and(|k| k.starts_with("EC")) {
        if out.pointer("/key_props/crv").is_none_or(Value::is_null) {
            out["key_props"]["crv"] = json!("P-256");
        }
        if let Some(props) = out["key_props"].as_object_mut() {
            props.remove("key_size");
        }
        if given.get("x509_props").and_then(|x| x.get("key_usage")).is_none() {
            out["x509_props"]["key_usage"] = json!(["digitalSignature"]);
        }
    }
    out
}

/// A key for a policy's `key_props`.
fn new_material(policy: &Value) -> Result<Material, ArmError> {
    let kty = policy.pointer("/key_props/kty").and_then(Value::as_str).unwrap_or("RSA");
    match kty {
        "RSA" | "RSA-HSM" => {
            let size = policy.pointer("/key_props/key_size").and_then(Value::as_i64).unwrap_or(2048);
            if ![2048, 3072, 4096].contains(&size) {
                return Err(bad(format!("Invalid key size {size}. Supported sizes are 2048, 3072 and 4096.")));
            }
            Material::generate("RSA", Some(size), None, None).map_err(bad)
        }
        "EC" | "EC-HSM" => match policy.pointer("/key_props/crv").and_then(Value::as_str).unwrap_or("P-256") {
            curve @ ("P-256" | "P-384") => Material::generate("EC", None, Some(curve), None).map_err(bad),
            other => Err(bad(format!("The curve {other} is not supported for certificates by the emulator; use P-256 or P-384."))),
        },
        other => Err(bad(format!("Invalid key type {other} for a certificate."))),
    }
}

/// `CN=a, O=b` as a distinguished name.
fn distinguished_name(subject: &str) -> Result<rcgen::DistinguishedName, ArmError> {
    use rcgen::DnType;
    let mut name = rcgen::DistinguishedName::new();
    for part in subject.split([',', ';']).map(str::trim).filter(|p| !p.is_empty()) {
        let (kind, value) = part.split_once('=').ok_or_else(|| bad(format!("The subject '{subject}' is not a distinguished name.")))?;
        let kind = match kind.trim().to_ascii_uppercase().as_str() {
            "CN" => DnType::CommonName,
            "O" => DnType::OrganizationName,
            "OU" => DnType::OrganizationalUnitName,
            "C" => DnType::CountryName,
            "L" => DnType::LocalityName,
            "ST" | "S" => DnType::StateOrProvinceName,
            other => return Err(bad(format!("The subject attribute '{other}' is not supported."))),
        };
        name.push(kind, value.trim());
    }
    Ok(name)
}

/// Certificate parameters from a policy's `x509_props`.
fn parameters(policy: &Value) -> Result<rcgen::CertificateParams, ArmError> {
    use rcgen::{ExtendedKeyUsagePurpose as Eku, KeyUsagePurpose as Ku, SanType};
    let x509 = policy.get("x509_props").cloned().unwrap_or_default();
    let mut params = rcgen::CertificateParams::default();
    let subject = x509.get("subject").and_then(Value::as_str).unwrap_or("");
    params.distinguished_name = distinguished_name(subject)?;
    let list = |path: &str| -> Vec<String> {
        x509.pointer(path).and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect()
    };
    for dns in list("/sans/dns_names") {
        params.subject_alt_names.push(SanType::DnsName(dns.try_into().map_err(|_| bad("A DNS name is not valid."))?));
    }
    for email in list("/sans/emails") {
        params.subject_alt_names.push(SanType::Rfc822Name(email.try_into().map_err(|_| bad("An email address is not valid."))?));
    }
    if subject.is_empty() && params.subject_alt_names.is_empty() {
        return Err(bad("The policy must have a subject or subject alternative names."));
    }
    params.key_usages = list("/key_usage")
        .iter()
        .filter_map(|usage| {
            Some(match usage.as_str() {
                "digitalSignature" => Ku::DigitalSignature,
                "nonRepudiation" => Ku::ContentCommitment,
                "keyEncipherment" => Ku::KeyEncipherment,
                "dataEncipherment" => Ku::DataEncipherment,
                "keyAgreement" => Ku::KeyAgreement,
                "keyCertSign" => Ku::KeyCertSign,
                "cRLSign" => Ku::CrlSign,
                "encipherOnly" => Ku::EncipherOnly,
                "decipherOnly" => Ku::DecipherOnly,
                _ => return None,
            })
        })
        .collect();
    params.extended_key_usages = list("/ekus")
        .iter()
        .filter_map(|oid| {
            Some(match oid.as_str() {
                "1.3.6.1.5.5.7.3.1" => Eku::ServerAuth,
                "1.3.6.1.5.5.7.3.2" => Eku::ClientAuth,
                "1.3.6.1.5.5.7.3.3" => Eku::CodeSigning,
                "1.3.6.1.5.5.7.3.4" => Eku::EmailProtection,
                "1.3.6.1.5.5.7.3.8" => Eku::TimeStamping,
                "1.3.6.1.5.5.7.3.9" => Eku::OcspSigning,
                other => Eku::Other(other.split('.').map(str::parse).collect::<Result<_, _>>().ok()?),
            })
        })
        .collect();
    let months = x509.get("validity_months").and_then(Value::as_i64).unwrap_or(12);
    if !(1..=1200).contains(&months) {
        return Err(bad("validity_months must be between 1 and 1200."));
    }
    let start = time::OffsetDateTime::now_utc().replace_nanosecond(0).unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    params.not_before = start;
    params.not_after = start + time::Duration::days(months * 30);
    Ok(params)
}

fn signer(material: &Material) -> Result<rcgen::KeyPair, ArmError> {
    let signer = CertificateSigner::new(material).map_err(bad)?;
    rcgen::KeyPair::from_remote(Box::new(signer)).map_err(|e| bad(e.to_string()))
}

/// What a certificate says about itself.
struct Parsed {
    subject: String,
    not_before: i64,
    expires: i64,
    spki: Vec<u8>,
    dns_names: Vec<String>,
    emails: Vec<String>,
}

fn parse(der: &[u8]) -> Result<Parsed, ArmError> {
    use x509_parser::extensions::GeneralName;
    let (_, certificate) = x509_parser::parse_x509_certificate(der).map_err(|_| bad("The certificate is not a valid X.509 certificate."))?;
    let (mut dns_names, mut emails) = (Vec::new(), Vec::new());
    if let Ok(Some(sans)) = certificate.subject_alternative_name() {
        for name in &sans.value.general_names {
            match name {
                GeneralName::DNSName(dns) => dns_names.push(dns.to_string()),
                GeneralName::RFC822Name(email) => emails.push(email.to_string()),
                _ => {}
            }
        }
    }
    Ok(Parsed {
        subject: certificate.subject().to_string(),
        not_before: certificate.validity().not_before.timestamp(),
        expires: certificate.validity().not_after.timestamp(),
        spki: certificate.public_key().raw.to_vec(),
        dns_names,
        emails,
    })
}

fn pem_block(tag: &str, der: &[u8]) -> String {
    pem::encode_config(&pem::Pem::new(tag, der.to_vec()), pem::EncodeConfig::new().set_line_ending(pem::LineEnding::LF))
}

/// The secret a certificate keeps: its key and certificates as PKCS#12
/// (base64, no password) or PEM.
fn secret_value(content_type: &str, material: &Material, der: &[u8], chain: &[Vec<u8>]) -> Result<String, ArmError> {
    let key = cert_crypto::pkcs8_der(material).map_err(bad)?;
    if content_type == PEM {
        let mut out = pem_block("PRIVATE KEY", &key);
        for certificate in std::iter::once(der).chain(chain.iter().map(Vec::as_slice)) {
            out.push_str(&pem_block("CERTIFICATE", certificate));
        }
        return Ok(out);
    }
    use p12_keystore::{Certificate as P12Certificate, KeyStore, KeyStoreEntry, PrivateKey, PrivateKeyChain};
    let failed = |e: p12_keystore::error::Error| bad(format!("The PKCS#12 archive could not be written: {e}"));
    let mut certificates = vec![P12Certificate::from_der(der).map_err(failed)?];
    for extra in chain {
        certificates.push(P12Certificate::from_der(extra).map_err(failed)?);
    }
    let local_key_id = sha1::Sha1::digest(der).to_vec();
    let entry = PrivateKeyChain::new(local_key_id, PrivateKey::from_der(&key).map_err(failed)?, certificates);
    let mut store = KeyStore::new();
    store.add_entry("certificate", KeyStoreEntry::PrivateKeyChain(entry));
    Ok(STANDARD.encode(store.writer("").write().map_err(failed)?))
}

pub(crate) struct CertificateRequest<'a> {
    pub engine: &'a Engine,
    pub vault: &'a str,
    pub body: &'a Value,
}

impl CertificateRequest<'_> {
    pub(crate) fn serve(&self, method: &Method, collection: &str, rest: &[&str]) -> Result<Answer, ArmError> {
        let lower: Vec<String> = rest.iter().map(|s| s.to_ascii_lowercase()).collect();
        let tail: Vec<&str> = lower.iter().map(String::as_str).collect();
        let ok = |result: Result<Value, ArmError>| result.map(|v| Answer::Json(StatusCode::OK, v));
        match (collection, method.as_str(), rest, tail.as_slice()) {
            ("certificates", "GET", [], _) => ok(self.list()),
            ("certificates", "PUT", _, ["contacts"]) => ok(self.set_contacts()),
            ("certificates", "GET", _, ["contacts"]) => ok(self.contacts(false)),
            ("certificates", "DELETE", _, ["contacts"]) => ok(self.contacts(true)),
            ("certificates", "GET", _, ["issuers"]) => ok(self.list_issuers()),
            ("certificates", "PUT", [_, name], ["issuers", _]) => ok(self.set_issuer(name, true)),
            ("certificates", "PATCH", [_, name], ["issuers", _]) => ok(self.set_issuer(name, false)),
            ("certificates", "GET", [_, name], ["issuers", _]) => ok(self.issuer(name, false)),
            ("certificates", "DELETE", [_, name], ["issuers", _]) => ok(self.issuer(name, true)),
            ("certificates", "POST", _, ["restore"]) => ok(self.restore()),
            ("certificates", "POST", [name, _], [_, "create"]) => self.create(name).map(|v| Answer::Json(StatusCode::ACCEPTED, v)),
            ("certificates", "POST", [name, _], [_, "import"]) => ok(self.import(name)),
            ("certificates", "POST", [name, _], [_, "backup"]) => ok(self.backup(name)),
            ("certificates", "GET", [name, _], [_, "versions"]) => ok(self.versions(name)),
            ("certificates", "GET", [name, _], [_, "policy"]) => ok(self.policy(name)),
            ("certificates", "PATCH", [name, _], [_, "policy"]) => ok(self.update_policy(name)),
            ("certificates", "GET", [name, _], [_, "pending"]) => ok(self.pending(name)),
            ("certificates", "PATCH", [name, _], [_, "pending"]) => ok(self.cancel(name)),
            ("certificates", "DELETE", [name, _], [_, "pending"]) => ok(self.delete_pending(name)),
            ("certificates", "POST", [name, _, _], [_, "pending", "merge"]) => self.merge(name).map(|v| Answer::Json(StatusCode::CREATED, v)),
            ("certificates", "GET", [name], _) => ok(self.get(name, None)),
            ("certificates", "GET", [name, version], _) => ok(self.get(name, Some(version))),
            ("certificates", "PATCH", [name], _) => ok(self.update(name, None)),
            ("certificates", "PATCH", [name, version], _) => ok(self.update(name, Some(version))),
            ("certificates", "DELETE", [name], _) => ok(self.delete(name)),
            ("deletedcertificates", "GET", [], _) => ok(self.list_deleted()),
            ("deletedcertificates", "GET", [name], _) => ok(self.get_deleted(name)),
            ("deletedcertificates", "DELETE", [name], _) => self.purge(name).map(|()| Answer::Empty(StatusCode::NO_CONTENT)),
            ("deletedcertificates", "POST", [name, _], [_, "recover"]) => ok(self.recover(name)),
            _ => Err(ArmError::not_implemented("this Key Vault operation")),
        }
    }

    fn base(&self) -> String {
        self.engine.config().vault_uri(self.vault)
    }

    fn with_vault<T>(&self, f: impl FnOnce(&mut Vault) -> Result<T, ArmError>) -> Result<T, ArmError> {
        let state = self.engine.state::<KeyVaultState>();
        let mut state = state.lock();
        let vault = state.vaults.get_mut(self.vault).ok_or_else(|| ArmError::not_found("VaultNotFound", "The vault was not found."))?;
        f(vault)
    }

    fn policy_json(&self, certificate: &Certificate) -> Value {
        let mut policy = certificate.policy.clone();
        policy["id"] = json!(format!("{}certificates/{}/policy", self.base(), certificate.name));
        policy
    }

    fn attributes(version: &CertificateVersion) -> Value {
        json!({
            "enabled": version.enabled,
            "nbf": version.not_before,
            "exp": version.expires,
            "created": version.created,
            "updated": version.updated,
            "recoveryLevel": "Recoverable+Purgeable",
            "recoverableDays": RECOVERABLE_DAYS,
        })
    }

    fn bundle(&self, certificate: &Certificate, version: &CertificateVersion) -> Value {
        let base = self.base();
        let name = &certificate.name;
        let mut out = json!({
            "id": format!("{base}certificates/{name}/{}", version.version),
            "kid": format!("{base}keys/{name}/{}", version.version),
            "sid": format!("{base}secrets/{name}/{}", version.version),
            "x5t": URL_SAFE_NO_PAD.encode(sha1::Sha1::digest(&version.der)),
            "cer": STANDARD.encode(&version.der),
            "contentType": certificate.policy.pointer("/secret_props/contentType").cloned().unwrap_or(json!(PKCS12)),
            "attributes": Self::attributes(version),
            "policy": self.policy_json(certificate),
        });
        if let Some(tags) = &version.tags {
            out["tags"] = tags.clone();
        }
        out
    }

    fn item(&self, certificate: &Certificate, version: &CertificateVersion, versioned: bool) -> Value {
        let mut id = format!("{}certificates/{}", self.base(), certificate.name);
        if versioned {
            id.push_str(&format!("/{}", version.version));
        }
        let mut out = json!({
            "id": id,
            "x5t": URL_SAFE_NO_PAD.encode(sha1::Sha1::digest(&version.der)),
            "attributes": Self::attributes(version),
        });
        if let Some(tags) = &version.tags {
            out["tags"] = tags.clone();
        }
        out
    }

    fn operation_json(&self, certificate: &Certificate, operation: &Operation) -> Value {
        let base = self.base();
        let mut out = json!({
            "id": format!("{base}certificates/{}/pending", certificate.name),
            "issuer": {"name": operation.issuer},
            "cancellation_requested": operation.cancellation_requested,
            "status": operation.status,
            "status_details": match operation.status {
                "inProgress" => "Pending certificate created. Please Perform Merge to complete the request.",
                "cancelled" => "The certificate operation was cancelled.",
                _ => "",
            },
            "request_id": operation.request_id,
        });
        if let Some(csr) = &operation.csr {
            out["csr"] = json!(STANDARD.encode(csr));
        }
        if operation.status == "completed" {
            out["target"] = json!(format!("{base}certificates/{}", certificate.name));
        }
        out
    }

    fn list(&self) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let items: Vec<Value> = vault.certificates.values().filter_map(|c| c.versions.last().map(|v| self.item(c, v, false))).collect();
            Ok(json!({"value": items, "nextLink": null}))
        })
    }

    fn versions(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let certificate = vault.certificates.get(&name.to_ascii_lowercase()).ok_or_else(|| not_found(name))?;
            let items: Vec<Value> = certificate.versions.iter().map(|v| self.item(certificate, v, true)).collect();
            Ok(json!({"value": items, "nextLink": null}))
        })
    }

    fn get(&self, name: &str, version: Option<&str>) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let certificate = vault.certificates.get(&name.to_ascii_lowercase()).ok_or_else(|| not_found(name))?;
            let found = match version.filter(|v| !v.is_empty()) {
                Some(wanted) => certificate.versions.iter().find(|v| v.version == wanted),
                None => certificate.versions.last(),
            }
            .ok_or_else(|| not_found(name))?;
            Ok(self.bundle(certificate, found))
        })
    }

    fn update(&self, name: &str, version: Option<&str>) -> Result<Value, ArmError> {
        let enabled = self.body.pointer("/attributes/enabled").and_then(Value::as_bool);
        let tags = self.body.get("tags").cloned();
        let policy = self.body.get("policy").cloned();
        self.with_vault(|vault| {
            let certificate = vault.certificates.get_mut(&name.to_ascii_lowercase()).ok_or_else(|| not_found(name))?;
            let index = match version.filter(|v| !v.is_empty()) {
                Some(wanted) => certificate.versions.iter().position(|v| v.version == wanted).ok_or_else(|| not_found(name))?,
                None => certificate.versions.len().checked_sub(1).ok_or_else(|| not_found(name))?,
            };
            if let Some(policy) = policy.filter(|p| !p.is_null()) {
                certificate.policy = merge_policy(&certificate.policy, Some(&policy));
            }
            let target = &mut certificate.versions[index];
            if let Some(enabled) = enabled {
                target.enabled = enabled;
            }
            if let Some(tags) = tags {
                target.tags = (!tags.is_null()).then_some(tags);
            }
            target.updated = now();
            Ok(self.bundle(certificate, &certificate.versions[index]))
        })
    }

    fn policy(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let certificate = vault.certificates.get(&name.to_ascii_lowercase()).ok_or_else(|| not_found(name))?;
            Ok(self.policy_json(certificate))
        })
    }

    fn update_policy(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let certificate = vault.certificates.get_mut(&name.to_ascii_lowercase()).ok_or_else(|| not_found(name))?;
            certificate.policy = merge_policy(&certificate.policy, Some(self.body));
            certificate.policy["attributes"]["updated"] = json!(now());
            Ok(self.policy_json(certificate))
        })
    }

    /// Create Certificate: with the `Self` issuer the certificate is made at
    /// once; with `Unknown`, a key and CSR wait for a merge.
    fn create(&self, name: &str) -> Result<Value, ArmError> {
        let existing = self.with_vault(|vault| {
            let lower = name.to_ascii_lowercase();
            if vault.deleted_certificates.contains_key(&lower) {
                return Err(in_deleted_state(name));
            }
            if vault.certificates.get(&lower).and_then(|c| c.operation.as_ref()).is_some_and(|o| o.status == "inProgress") {
                return Err(ArmError::conflict("Conflict", format!("There is a pending operation on certificate {name}.")));
            }
            Ok(vault.certificates.get(&lower).map(|c| c.policy.clone()))
        })?;
        let base = existing.unwrap_or_else(default_policy);
        let mut policy = merge_policy(&base, self.body.get("policy"));
        let stamp = now();
        policy["attributes"]["created"] = base.pointer("/attributes/created").cloned().unwrap_or(json!(stamp));
        policy["attributes"]["updated"] = json!(stamp);
        let issuer = policy.pointer("/issuer/name").and_then(Value::as_str).unwrap_or("Unknown").to_string();
        if !issuer.eq_ignore_ascii_case("Self") && !issuer.eq_ignore_ascii_case("Unknown") {
            return Err(bad(format!("The emulator issues certificates only with the Self and Unknown issuers, not '{issuer}'.")));
        }
        let params = parameters(&policy)?;
        let material = new_material(&policy)?;
        let key_pair = signer(&material)?;
        let enabled = self.body.pointer("/attributes/enabled").and_then(Value::as_bool).unwrap_or(true);
        let tags = self.body.get("tags").filter(|t| !t.is_null()).cloned();
        let version = uuid::Uuid::new_v4().simple().to_string();
        let mut operation = Operation {
            status: "inProgress",
            issuer: issuer.clone(),
            version: version.clone(),
            material: None,
            csr: None,
            cancellation_requested: false,
            request_id: uuid::Uuid::new_v4().simple().to_string(),
            enabled,
            tags: tags.clone(),
        };
        let issued = if issuer.eq_ignore_ascii_case("Self") {
            let mut params = params;
            let mut serial = [0u8; 16];
            rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut serial);
            serial[0] &= 0x7f;
            params.serial_number = Some(rcgen::SerialNumber::from(serial.to_vec()));
            let certificate = params.self_signed(&key_pair).map_err(|e| bad(e.to_string()))?;
            operation.status = "completed";
            Some(certificate.der().to_vec())
        } else {
            let request = params.serialize_request(&key_pair).map_err(|e| bad(e.to_string()))?;
            operation.csr = Some(request.der().to_vec());
            operation.material = Some(material.clone());
            None
        };
        self.with_vault(|vault| {
            let lower = name.to_ascii_lowercase();
            let certificate = vault
                .certificates
                .entry(lower.clone())
                .or_insert_with(|| Certificate { name: name.to_string(), versions: Vec::new(), policy: Value::Null, operation: None });
            certificate.policy = policy;
            certificate.operation = Some(operation);
            if let Some(der) = issued {
                let version = CertificateVersion::new(&version, der, Vec::new(), enabled, tags)?;
                complete(&self.base(), vault, name, version, material)?;
            }
            // The answer is always `inProgress`, as Azure's is: SDK pollers
            // never poll an operation whose first answer is final, and then
            // never fetch the certificate. The first poll sees `completed`.
            let certificate = &vault.certificates[&lower];
            let mut body = self.operation_json(certificate, certificate.operation.as_ref().expect("just set"));
            body["status"] = json!("inProgress");
            if let Some(members) = body.as_object_mut() {
                members.remove("target");
            }
            Ok(body)
        })
    }

    fn pending(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let certificate = vault.certificates.get(&name.to_ascii_lowercase()).ok_or_else(|| not_found(name))?;
            let operation = certificate.operation.as_ref().ok_or_else(|| pending_not_found(name))?;
            Ok(self.operation_json(certificate, operation))
        })
    }

    fn cancel(&self, name: &str) -> Result<Value, ArmError> {
        let requested = self.body.get("cancellation_requested").and_then(Value::as_bool).unwrap_or(false);
        self.with_vault(|vault| {
            let certificate = vault.certificates.get_mut(&name.to_ascii_lowercase()).ok_or_else(|| not_found(name))?;
            let operation = certificate.operation.as_mut().ok_or_else(|| pending_not_found(name))?;
            if requested && operation.status == "inProgress" {
                operation.cancellation_requested = true;
                operation.status = "cancelled";
                operation.material = None;
            }
            let certificate = &vault.certificates[&name.to_ascii_lowercase()];
            Ok(self.operation_json(certificate, certificate.operation.as_ref().expect("checked above")))
        })
    }

    fn delete_pending(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let lower = name.to_ascii_lowercase();
            let certificate = vault.certificates.get_mut(&lower).ok_or_else(|| not_found(name))?;
            let operation = certificate.operation.take().ok_or_else(|| pending_not_found(name))?;
            let body = self.operation_json(certificate, &operation);
            // A certificate that never got a version goes with its operation.
            if certificate.versions.is_empty() {
                vault.certificates.remove(&lower);
            }
            Ok(body)
        })
    }

    /// Merge Certificate: completes an `Unknown`-issuer request with the
    /// certificate a CA signed for its key.
    fn merge(&self, name: &str) -> Result<Value, ArmError> {
        let chain: Vec<Vec<u8>> = self
            .body
            .get("x5c")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|c| c.as_str().and_then(|c| STANDARD.decode(c).ok().or_else(|| unb64(c).ok())).ok_or_else(|| bad("x5c must hold base64 certificates.")))
            .collect::<Result<_, _>>()?;
        let (leaf, rest) = chain.split_first().ok_or_else(|| bad("x5c must hold the signed certificate."))?;
        let parsed = parse(leaf)?;
        let enabled = self.body.pointer("/attributes/enabled").and_then(Value::as_bool);
        let tags = self.body.get("tags").filter(|t| !t.is_null()).cloned();
        self.with_vault(|vault| {
            let certificate = vault.certificates.get_mut(&name.to_ascii_lowercase()).ok_or_else(|| not_found(name))?;
            let pending = certificate.operation.as_mut().filter(|o| o.status == "inProgress").ok_or_else(|| pending_not_found(name))?;
            let material = pending.material.clone().ok_or_else(|| bad("The pending certificate has no key."))?;
            if cert_crypto::spki_der(&material).map_err(bad)? != parsed.spki {
                return Err(bad("Public key from x5c content does not match with key of the pending certificate."));
            }
            pending.status = "completed";
            pending.material = None;
            let version = CertificateVersion::new(&pending.version, leaf.to_vec(), rest.to_vec(), enabled.unwrap_or(pending.enabled), tags.or(pending.tags.clone()))?;
            complete(&self.base(), vault, name, version, material)?;
            let certificate = &vault.certificates[&name.to_ascii_lowercase()];
            Ok(self.bundle(certificate, certificate.versions.last().expect("just added")))
        })
    }

    /// Import Certificate: a PKCS#12 archive (base64, with `pwd` if it has
    /// one) or PEM, holding the private key and the certificate.
    fn import(&self, name: &str) -> Result<Value, ArmError> {
        let value = self.body.get("value").and_then(Value::as_str).ok_or_else(|| bad("The certificate to import (value) is required."))?;
        let password = self.body.get("pwd").and_then(Value::as_str).unwrap_or("");
        // PEM comes as text or, from the SDKs, base64-encoded like PKCS#12.
        let decoded = STANDARD.decode(value.trim()).ok();
        let pem_text = match &decoded {
            _ if value.contains("-----BEGIN") => Some(value.to_string()),
            Some(bytes) if bytes.starts_with(b"-----BEGIN") => String::from_utf8(bytes.clone()).ok(),
            _ => None,
        };
        let (material, mut certificates, given_type) = if let Some(text) = pem_text {
            let blocks = pem::parse_many(&text).map_err(|_| bad("The PEM is not valid."))?;
            let key = blocks.iter().find(|b| b.tag().ends_with("PRIVATE KEY")).ok_or_else(|| bad("The PEM has no private key."))?;
            let material = cert_crypto::from_private_der(key.contents()).map_err(bad)?;
            let certificates: Vec<Vec<u8>> = blocks.iter().filter(|b| b.tag() == "CERTIFICATE").map(|b| b.contents().to_vec()).collect();
            (material, certificates, PEM)
        } else {
            let archive = decoded.ok_or_else(|| bad("The certificate is neither PEM nor base64 PKCS#12."))?;
            let store = p12_keystore::KeyStore::from_pkcs12(&archive, password, p12_keystore::Pkcs12ImportPolicy::Relaxed)
                .map_err(|e| bad(format!("The PKCS#12 archive could not be read: {e}")))?;
            let (_, chain) = store.private_key_chain().ok_or_else(|| bad("The PKCS#12 archive has no private key."))?;
            let material = cert_crypto::from_private_der(chain.key().as_der()).map_err(bad)?;
            let certificates = chain.certs().iter().map(|c| c.as_der().to_vec()).collect();
            (material, certificates, PKCS12)
        };
        // The leaf is the certificate whose public key is the private key's.
        let spki = cert_crypto::spki_der(&material).map_err(bad)?;
        let leaf_index = certificates
            .iter()
            .position(|c| parse(c).is_ok_and(|p| p.spki == spki))
            .ok_or_else(|| bad("No certificate matches the private key."))?;
        let leaf = certificates.remove(leaf_index);
        let parsed = parse(&leaf)?;

        let mut key_props = json!({"exportable": true, "reuse_key": false, "kty": material.family()});
        match (material.curve(), material.key_size()) {
            (Some(curve), _) => key_props["crv"] = json!(curve),
            (None, Some(bits)) => key_props["key_size"] = json!(bits),
            _ => {}
        }
        let months = ((parsed.expires - parsed.not_before) / (30 * 86_400)).max(1);
        let mut x509 = json!({"subject": parsed.subject, "validity_months": months});
        if !parsed.dns_names.is_empty() || !parsed.emails.is_empty() {
            x509["sans"] = json!({"dns_names": parsed.dns_names, "emails": parsed.emails});
        }
        let derived = json!({
            "key_props": key_props,
            "secret_props": {"contentType": given_type},
            "x509_props": x509,
            "issuer": {"name": "Unknown"},
            "attributes": {"enabled": true},
        });
        let mut policy = merge_policy(&derived, self.body.get("policy"));
        let stamp = now();
        policy["attributes"]["created"] = json!(stamp);
        policy["attributes"]["updated"] = json!(stamp);
        let enabled = self.body.pointer("/attributes/enabled").and_then(Value::as_bool).unwrap_or(true);
        let tags = self.body.get("tags").filter(|t| !t.is_null()).cloned();
        let version = CertificateVersion::new(&uuid::Uuid::new_v4().simple().to_string(), leaf, certificates, enabled, tags)?;
        self.with_vault(|vault| {
            let lower = name.to_ascii_lowercase();
            if vault.deleted_certificates.contains_key(&lower) {
                return Err(in_deleted_state(name));
            }
            let certificate = vault
                .certificates
                .entry(lower.clone())
                .or_insert_with(|| Certificate { name: name.to_string(), versions: Vec::new(), policy: Value::Null, operation: None });
            let created = certificate.policy.pointer("/attributes/created").cloned();
            certificate.policy = policy;
            if let Some(created) = created {
                certificate.policy["attributes"]["created"] = created;
            }
            complete(&self.base(), vault, name, version, material)?;
            let certificate = &vault.certificates[&lower];
            Ok(self.bundle(certificate, certificate.versions.last().expect("just added")))
        })
    }

    fn delete(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let lower = name.to_ascii_lowercase();
            let certificate = vault.certificates.remove(&lower).ok_or_else(|| not_found(name))?;
            let deleted = now();
            let body = self.deleted_bundle(&certificate, deleted);
            let key = vault.keys.remove(&lower);
            let secret = vault.take_secret(name);
            vault.deleted_certificates.insert(lower, DeletedCertificate { certificate, key, secret, deleted });
            Ok(body)
        })
    }

    fn deleted_bundle(&self, certificate: &Certificate, deleted: i64) -> Value {
        let mut out = match certificate.versions.last() {
            Some(version) => self.bundle(certificate, version),
            None => json!({"id": format!("{}certificates/{}", self.base(), certificate.name), "policy": self.policy_json(certificate)}),
        };
        out["recoveryId"] = json!(format!("{}deletedcertificates/{}", self.base(), certificate.name));
        out["deletedDate"] = json!(deleted);
        out["scheduledPurgeDate"] = json!(deleted + RECOVERABLE_DAYS * 86_400);
        out
    }

    fn list_deleted(&self) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let items: Vec<Value> = vault
                .deleted_certificates
                .values()
                .filter_map(|d| {
                    let mut item = self.item(&d.certificate, d.certificate.versions.last()?, false);
                    item["recoveryId"] = json!(format!("{}deletedcertificates/{}", self.base(), d.certificate.name));
                    item["deletedDate"] = json!(d.deleted);
                    item["scheduledPurgeDate"] = json!(d.deleted + RECOVERABLE_DAYS * 86_400);
                    Some(item)
                })
                .collect();
            Ok(json!({"value": items, "nextLink": null}))
        })
    }

    fn get_deleted(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let deleted = vault.deleted_certificates.get(&name.to_ascii_lowercase()).ok_or_else(|| deleted_not_found(name))?;
            Ok(self.deleted_bundle(&deleted.certificate, deleted.deleted))
        })
    }

    fn purge(&self, name: &str) -> Result<(), ArmError> {
        self.with_vault(|vault| vault.deleted_certificates.remove(&name.to_ascii_lowercase()).map(|_| ()).ok_or_else(|| deleted_not_found(name)))
    }

    fn recover(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let lower = name.to_ascii_lowercase();
            if vault.certificates.contains_key(&lower) {
                return Err(ArmError::conflict("Conflict", format!("Certificate {name} already exists.")));
            }
            let deleted = vault.deleted_certificates.remove(&lower).ok_or_else(|| deleted_not_found(name))?;
            if let Some(key) = deleted.key {
                vault.keys.insert(lower.clone(), key);
            }
            if let Some(secret) = deleted.secret {
                vault.put_secret(secret);
            }
            let certificate = deleted.certificate;
            let body = match certificate.versions.last() {
                Some(version) => self.bundle(&certificate, version),
                None => json!({}),
            };
            vault.certificates.insert(lower, certificate);
            Ok(body)
        })
    }

    /// Backup Certificate: every version with its key, and the policy.
    fn backup(&self, name: &str) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let lower = name.to_ascii_lowercase();
            let certificate = vault.certificates.get(&lower).ok_or_else(|| not_found(name))?;
            let key = vault.keys.get(&lower);
            let mut versions = Vec::new();
            for version in &certificate.versions {
                let jwk = key
                    .and_then(|k| k.versions.iter().find(|v| v.version == version.version))
                    .and_then(|v| v.material.as_ref())
                    .map(Material::private_jwk)
                    .ok_or_else(|| bad("The certificate's key is missing."))?;
                versions.push(json!({
                    "version": version.version,
                    "der": STANDARD.encode(&version.der),
                    "chain": version.chain.iter().map(|c| STANDARD.encode(c)).collect::<Vec<_>>(),
                    "enabled": version.enabled,
                    "tags": version.tags,
                    "jwk": jwk,
                }));
            }
            let blob = json!({"format": BACKUP_MARKER, "name": certificate.name, "policy": certificate.policy, "versions": versions});
            Ok(json!({"value": URL_SAFE_NO_PAD.encode(blob.to_string())}))
        })
    }

    fn restore(&self) -> Result<Value, ArmError> {
        let malformed = || bad("Backup blob contains invalid or corrupt version.");
        let blob: Value = self
            .body
            .get("value")
            .and_then(Value::as_str)
            .and_then(|t| unb64(t).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .filter(|b: &Value| b.get("format").and_then(Value::as_str) == Some(BACKUP_MARKER))
            .ok_or_else(malformed)?;
        let name = blob.get("name").and_then(Value::as_str).ok_or_else(malformed)?.to_string();
        let mut versions = Vec::new();
        for v in blob.get("versions").and_then(Value::as_array).ok_or_else(malformed)? {
            let decode = |text: Option<&Value>| text.and_then(Value::as_str).and_then(|t| STANDARD.decode(t).ok()).ok_or_else(malformed);
            let material = Material::from_jwk(v.get("jwk").ok_or_else(malformed)?).map_err(|_| malformed())?;
            let chain = v.get("chain").and_then(Value::as_array).into_iter().flatten().map(|c| decode(Some(c))).collect::<Result<Vec<_>, _>>()?;
            let version = CertificateVersion::new(
                v.get("version").and_then(Value::as_str).ok_or_else(malformed)?,
                decode(v.get("der"))?,
                chain,
                v.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                v.get("tags").filter(|t| !t.is_null()).cloned(),
            )?;
            versions.push((version, material));
        }
        if versions.is_empty() {
            return Err(malformed());
        }
        self.with_vault(|vault| {
            let lower = name.to_ascii_lowercase();
            if vault.certificates.contains_key(&lower) || vault.keys.contains_key(&lower) {
                return Err(ArmError::conflict("Conflict", format!("Certificate {name} already exists.")));
            }
            if vault.deleted_certificates.contains_key(&lower) {
                return Err(in_deleted_state(&name));
            }
            let policy = blob.get("policy").cloned().unwrap_or_else(default_policy);
            vault.certificates.insert(lower.clone(), Certificate { name: name.clone(), versions: Vec::new(), policy, operation: None });
            for (version, material) in versions {
                complete(&self.base(), vault, &name, version, material)?;
            }
            let certificate = &vault.certificates[&lower];
            Ok(self.bundle(certificate, certificate.versions.last().expect("checked above")))
        })
    }

    fn set_contacts(&self) -> Result<Value, ArmError> {
        let contacts = self.body.get("contacts").filter(|c| c.is_array()).cloned().ok_or_else(|| bad("contacts must be a list."))?;
        let body = json!({"id": format!("{}certificates/contacts", self.base()), "contacts": contacts});
        self.with_vault(|vault| {
            vault.certificate_contacts = Some(body.clone());
            Ok(body)
        })
    }

    fn contacts(&self, delete: bool) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let missing = || ArmError::not_found("ContactsNotFound", "Contacts not found");
            match delete {
                true => vault.certificate_contacts.take().ok_or_else(missing),
                false => vault.certificate_contacts.clone().ok_or_else(missing),
            }
        })
    }

    fn issuer_json(&self, name: &str, stored: &Value) -> Value {
        let mut out = stored.clone();
        out["id"] = json!(format!("{}certificates/issuers/{name}", self.base()));
        // The password is write-only.
        if let Some(credentials) = out.get_mut("credentials").and_then(Value::as_object_mut) {
            credentials.remove("pwd");
        }
        out
    }

    fn set_issuer(&self, name: &str, replace: bool) -> Result<Value, ArmError> {
        let stamp = now();
        self.with_vault(|vault| {
            let lower = name.to_ascii_lowercase();
            let mut stored = match (replace, vault.certificate_issuers.get(&lower)) {
                (false, None) => return Err(ArmError::not_found("CertificateIssuerNotFound", format!("Issuer not found: {name}"))),
                (false, Some(current)) => merge_policy(current, Some(self.body)),
                (true, current) => {
                    let mut fresh = merge_policy(&json!({"attributes": {"enabled": true}}), Some(self.body));
                    fresh["attributes"]["created"] = current.and_then(|c| c.pointer("/attributes/created").cloned()).unwrap_or(json!(stamp));
                    fresh
                }
            };
            if stored.get("provider").and_then(Value::as_str).is_none() {
                return Err(bad("The issuer's provider is required."));
            }
            if stored.pointer("/attributes/created").is_none() {
                stored["attributes"]["created"] = json!(stamp);
            }
            stored["attributes"]["updated"] = json!(stamp);
            let body = self.issuer_json(name, &stored);
            vault.certificate_issuers.insert(lower, stored);
            Ok(body)
        })
    }

    fn issuer(&self, name: &str, delete: bool) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let lower = name.to_ascii_lowercase();
            let missing = || ArmError::not_found("CertificateIssuerNotFound", format!("Issuer not found: {name}"));
            let stored = if delete { vault.certificate_issuers.remove(&lower) } else { vault.certificate_issuers.get(&lower).cloned() };
            Ok(self.issuer_json(name, &stored.ok_or_else(missing)?))
        })
    }

    fn list_issuers(&self) -> Result<Value, ArmError> {
        self.with_vault(|vault| {
            let items: Vec<Value> = vault
                .certificate_issuers
                .iter()
                .map(|(name, stored)| json!({"id": format!("{}certificates/issuers/{name}", self.base()), "provider": stored.get("provider").cloned().unwrap_or(Value::Null)}))
                .collect();
            Ok(json!({"value": items, "nextLink": null}))
        })
    }
}

impl CertificateVersion {
    fn new(version: &str, der: Vec<u8>, chain: Vec<Vec<u8>>, enabled: bool, tags: Option<Value>) -> Result<Self, ArmError> {
        let parsed = parse(&der)?;
        let created = now();
        Ok(Self { version: version.to_string(), der, chain, enabled, tags, created, updated: created, not_before: parsed.not_before, expires: parsed.expires })
    }
}

/// Adds a finished version: the certificate, its key and its secret, all
/// under the same version.
fn complete(base: &str, vault: &mut Vault, name: &str, version: CertificateVersion, material: Material) -> Result<(), ArmError> {
    let lower = name.to_ascii_lowercase();
    if vault.deleted_keys.contains_key(&lower) {
        return Err(ArmError::conflict("Conflict", format!("A deleted key named {name} must be purged or recovered first.")));
    }
    let certificate = vault.certificates.get_mut(&lower).ok_or_else(|| not_found(name))?;
    let policy = certificate.policy.clone();
    let content_type = policy.pointer("/secret_props/contentType").and_then(Value::as_str).unwrap_or(PKCS12).to_string();
    let value = secret_value(&content_type, &material, &version.der, &version.chain)?;
    let hsm = policy.pointer("/key_props/kty").and_then(Value::as_str).is_some_and(|k| k.ends_with("-HSM"));
    let kty = format!("{}{}", material.family(), if hsm { "-HSM" } else { "" });
    let mut attributes = Map::new();
    attributes.insert("enabled".into(), json!(version.enabled));
    attributes.insert("nbf".into(), json!(version.not_before));
    attributes.insert("exp".into(), json!(version.expires));
    if let Some(exportable) = policy.pointer("/key_props/exportable").filter(|v| v.is_boolean()) {
        attributes.insert("exportable".into(), exportable.clone());
    }
    let key_version = KeyVersion {
        version: version.version.clone(),
        ops: applicable_ops(&kty).iter().map(|op| op.to_string()).collect(),
        kty,
        key_size: material.key_size(),
        curve: material.curve().map(str::to_string),
        attributes,
        tags: None,
        rotation_policy: None,
        release_policy: None,
        created: version.created,
        updated: version.created,
        material: Some(material),
    };
    let kid = format!("{base}keys/{name}/{}", version.version);
    let (validity, version_id) = ((version.not_before, version.expires), version.version.clone());
    certificate.versions.push(version);
    let key = vault.keys.entry(lower).or_insert_with(|| Key { name: name.to_string(), versions: Vec::new() });
    key.versions.push(key_version);
    vault.add_certificate_secret(name, &version_id, value, &content_type, validity, kid);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_signed_certificates_carry_the_policy() {
        for kty in [json!({"kty": "RSA", "key_size": 2048}), json!({"kty": "EC", "crv": "P-384"})] {
            let policy = merge_policy(
                &default_policy(),
                Some(&json!({"key_props": kty, "x509_props": {"subject": "CN=example.test, O=Emulator", "sans": {"dns_names": ["www.example.test"]}}})),
            );
            let material = new_material(&policy).unwrap();
            let der = parameters(&policy).unwrap().self_signed(&signer(&material).unwrap()).unwrap().der().to_vec();
            let parsed = parse(&der).unwrap();
            assert_eq!(parsed.subject, "CN=example.test, O=Emulator");
            assert_eq!(parsed.dns_names, vec!["www.example.test"]);
            assert_eq!(parsed.spki, cert_crypto::spki_der(&material).unwrap());
            assert!(parsed.expires - parsed.not_before >= 359 * 86_400);
            for content_type in [PKCS12, PEM] {
                assert!(!secret_value(content_type, &material, &der, &[]).unwrap().is_empty());
            }
            let key = cert_crypto::pkcs8_der(&material).unwrap();
            assert_eq!(cert_crypto::spki_der(&cert_crypto::from_private_der(&key).unwrap()).unwrap(), parsed.spki);
        }
    }

    #[test]
    fn subjects_must_be_names() {
        assert!(distinguished_name("CN=a, O=b, OU=c, C=US").is_ok());
        assert!(distinguished_name("not a name").is_err());
    }
}
