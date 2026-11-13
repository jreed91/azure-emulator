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
    /// Port of the Service Bus AMQP listener (plain AMQP, as the SDKs'
    /// development emulator mode expects).
    pub amqp_port: u16,
    /// Port of the Service Bus AMQP-over-TLS (`amqps`) listener.
    pub amqps_port: u16,
    /// Suffix under which per-resource data-plane hostnames live.
    pub dns_suffix: String,
    /// Whether Resource Manager requests need a role assignment that permits
    /// them. Off by default: any valid token is allowed everything.
    pub enforce_rbac: bool,
    /// Object ids (for the emulator's tokens, client ids) treated as Owner of the
    /// subscription when role assignments are enforced.
    pub rbac_owners: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tenant_id: DEFAULT_TENANT_ID.into(),
            subscription_id: DEFAULT_SUBSCRIPTION_ID.into(),
            scheme: "https".into(),
            host: "localhost".into(),
            port: 10443,
            amqp_port: 5672,
            amqps_port: 5671,
            dns_suffix: "azure.localhost".into(),
            enforce_rbac: false,
            rbac_owners: Vec::new(),
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

    /// `.queue.core.<suffix>` — what a queue service hostname ends with.
    pub fn queue_host_suffix(&self) -> String {
        format!(".queue.core.{}", self.dns_suffix)
    }

    pub fn queue_endpoint(&self, account: &str) -> String {
        format!("{}://{}{}{}/", self.scheme, account, self.queue_host_suffix(), self.port_suffix())
    }

    /// `.file.core.<suffix>` — what a file service hostname ends with.
    pub fn file_host_suffix(&self) -> String {
        format!(".file.core.{}", self.dns_suffix)
    }

    pub fn file_endpoint(&self, account: &str) -> String {
        format!("{}://{}{}{}/", self.scheme, account, self.file_host_suffix(), self.port_suffix())
    }

    /// `.table.core.<suffix>` — what a table service hostname ends with.
    pub fn table_host_suffix(&self) -> String {
        format!(".table.core.{}", self.dns_suffix)
    }

    pub fn table_endpoint(&self, account: &str) -> String {
        format!("{}://{}{}{}/", self.scheme, account, self.table_host_suffix(), self.port_suffix())
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

    /// Address a managed HSM reports. Nothing serves its data plane yet; it is on
    /// the emulator's own domain so that clients never reach the real service.
    pub fn managed_hsm_uri(&self, name: &str) -> String {
        format!(
            "{}://{}.managedhsm.{}{}/",
            self.scheme,
            name,
            self.dns_suffix,
            self.port_suffix()
        )
    }

    /// The `Endpoint` of a Service Bus connection string: the AMQP listener,
    /// under the namespace's own hostname.
    pub fn servicebus_amqp_endpoint(&self, namespace: &str) -> String {
        format!("sb://{}.servicebus.{}:{}/", namespace, self.dns_suffix, self.amqp_port)
    }

    /// Address a Service Bus namespace reports. Nothing serves the messaging
    /// protocol there yet; it is on the emulator's own domain so that clients
    /// never reach the real service by mistake.
    pub fn servicebus_endpoint(&self, namespace: &str) -> String {
        format!(
            "https://{}.servicebus.{}{}/",
            namespace,
            self.dns_suffix,
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
