//! Emulator configuration and the endpoint addresses derived from it.
//!
//! Azure has no single-endpoint override like `AWS_ENDPOINT_URL`: data planes live on
//! per-resource hostnames. The emulator therefore serves everything on one port and
//! routes on the `Host` header:
//!
//! * `{host}` — Resource Manager and the Entra ID token service
//! * `{account}.blob.core.{dns_suffix}` — Blob storage
//! * `{vault}.vault.{dns_suffix}` — Key Vault

/// Subscription every emulator starts with.
pub const DEFAULT_SUBSCRIPTION_ID: &str = "00000000-0000-0000-0000-000000000000";
/// Tenant every emulator starts with.
pub const DEFAULT_TENANT_ID: &str = "11111111-1111-1111-1111-111111111111";

#[derive(Clone, Debug)]
pub struct Config {
    pub tenant_id: String,
    pub subscription_id: String,
    /// `https` or `http`; used when the emulator reports its own URLs.
    pub scheme: String,
    /// Hostname of the management and login endpoint.
    pub host: String,
    pub port: u16,
    /// Suffix under which per-resource data-plane hostnames live.
    pub dns_suffix: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tenant_id: DEFAULT_TENANT_ID.into(),
            subscription_id: DEFAULT_SUBSCRIPTION_ID.into(),
            scheme: "https".into(),
            host: "localhost".into(),
            port: 10443,
            dns_suffix: "azure.localhost".into(),
        }
    }
}

impl Config {
    fn port_suffix(&self) -> String {
        match (self.scheme.as_str(), self.port) {
            ("https", 443) | ("http", 80) => String::new(),
            (_, port) => format!(":{port}"),
        }
    }

    /// Base URL of Resource Manager and the token service, without a trailing slash.
    pub fn management_endpoint(&self) -> String {
        format!("{}://{}{}", self.scheme, self.host, self.port_suffix())
    }

    /// `.blob.core.<suffix>` — what a blob hostname ends with.
    pub fn blob_host_suffix(&self) -> String {
        format!(".blob.core.{}", self.dns_suffix)
    }

    /// `.vault.<suffix>` — what a vault hostname ends with.
    pub fn vault_host_suffix(&self) -> String {
        format!(".vault.{}", self.dns_suffix)
    }

    pub fn blob_endpoint(&self, account: &str) -> String {
        format!(
            "{}://{}{}{}/",
            self.scheme,
            account,
            self.blob_host_suffix(),
            self.port_suffix()
        )
    }

    pub fn vault_uri(&self, vault: &str) -> String {
        format!(
            "{}://{}{}{}/",
            self.scheme,
            vault,
            self.vault_host_suffix(),
            self.port_suffix()
        )
    }

    /// Audience a Key Vault token is requested for. It shares the vault hostname's
    /// domain (and port) so SDKs that verify the challenge resource against the vault accept it.
    pub fn vault_resource(&self) -> String {
        format!("https://vault.{}{}", self.dns_suffix, self.port_suffix())
    }

    /// Issuer of every token this emulator signs.
    pub fn issuer(&self) -> String {
        format!("https://sts.windows.net/{}/", self.tenant_id)
    }
}
