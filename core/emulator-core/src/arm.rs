//! The Azure Resource Manager (ARM) layer.
//!
//! ARM is one uniform protocol: every resource lives at
//! `/subscriptions/{sub}/resourceGroups/{rg}/providers/{Namespace}/{type}/{name}` and
//! shares the same envelope (`id`, `name`, `type`, `location`, `tags`, `properties`).
//! This module implements that protocol once — authentication, `api-version`
//! handling, subscriptions, resource groups, and generic create/read/update/delete/
//! list over a resource store — and a resource provider only supplies a
//! [`ResourceTypeHandler`] with the behaviour specific to its type.
//!
//! Path segments are matched case-insensitively, as ARM does.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use http::{header, HeaderValue, Method, Request, StatusCode};
use serde_json::{json, Map, Value};

use crate::auth::{self, AuthFailure};
use crate::http_util::{json_response, parse_query, percent_decode, read_body, request_id};
use crate::{ArmError, Engine};

mod authorization;
mod deployments;

pub use authorization::{data_access, principal_ids, AuthorizationState, Permission};

/// A stored ARM resource.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Resource {
    pub id: String,
    pub name: String,
    pub resource_type: String,
    pub location: Option<String>,
    pub tags: Option<Value>,
    pub kind: Option<Value>,
    pub sku: Option<Value>,
    pub identity: Option<Value>,
    pub properties: Value,
}

impl Resource {
    /// The full resource as ARM returns it.
    pub fn to_json(&self) -> Value {
        let mut out = self.summary();
        if let Some(identity) = &self.identity {
            out["identity"] = identity.clone();
        }
        out["properties"] = self.properties.clone();
        out
    }

    /// The envelope without `properties`, as generic resource listings return it.
    fn summary(&self) -> Value {
        let mut out = json!({"id": self.id, "name": self.name, "type": self.resource_type});
        for (key, value) in [
            ("location", self.location.clone().map(Value::String)),
            ("tags", self.tags.clone()),
            ("kind", self.kind.clone()),
            ("sku", self.sku.clone()),
        ] {
            if let Some(value) = value {
                out[key] = value;
            }
        }
        out
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct ResourceGroup {
    name: String,
    location: String,
    tags: Option<Value>,
    managed_by: Option<Value>,
}

/// Marks Resource Manager's states (resources, role assignments and custom
/// roles, deployments) as ones a snapshot holds.
pub fn persist(engine: &crate::Engine) {
    engine.persist::<ArmState>("arm");
    engine.persist::<authorization::AuthorizationState>("authorization");
    engine.persist::<deployments::DeploymentState>("deployments");
}

/// Resource Manager's own state: resource groups and every tracked resource.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct ArmState {
    /// Keyed by lowercased group name.
    groups: BTreeMap<String, ResourceGroup>,
    /// Keyed by lowercased resource id.
    resources: BTreeMap<String, Resource>,
    /// The client id of each system-assigned identity, by principal id.
    pub(crate) system_identity_clients: HashMap<String, String>,
}

impl ArmState {
    /// Looks a resource up by id, case-insensitively.
    pub fn resource(&self, id: &str) -> Option<&Resource> {
        self.resources.get(&id.to_ascii_lowercase())
    }

    /// Every stored resource, in id order.
    pub fn resources(&self) -> impl Iterator<Item = &Resource> {
        self.resources.values()
    }

    /// The client id of the system-assigned identity with `principal_id`.
    pub fn system_identity_client(&self, principal_id: &str) -> Option<&str> {
        self.system_identity_clients.get(principal_id).map(String::as_str)
    }

    /// Mutable access to a stored resource, for operations that change one outside
    /// the usual `PUT`/`PATCH` (an access-policy update, for example).
    pub fn resource_mut(&mut self, id: &str) -> Option<&mut Resource> {
        self.resources.get_mut(&id.to_ascii_lowercase())
    }

    /// Stores a resource a handler creates on the service's behalf (a default
    /// child, for example), replacing any resource with the same id.
    pub fn insert(&mut self, resource: Resource) {
        self.resources.insert(resource.id.to_ascii_lowercase(), resource);
    }

    /// Removes a resource a handler manages on the service's behalf. Its handler's
    /// `deleted` hook is not called.
    pub fn remove(&mut self, id: &str) -> Option<Resource> {
        self.resources.remove(&id.to_ascii_lowercase())
    }

    /// The resources directly inside the collection `collection_id`, e.g.
    /// `{topic id}/subscriptions`.
    pub fn children<'a>(&'a self, collection_id: &str) -> impl Iterator<Item = &'a Resource> + 'a {
        let prefix = format!("{}/", collection_id.to_ascii_lowercase());
        let from = prefix.clone();
        let skip = prefix.len();
        self.resources
            .range(from..)
            .take_while(move |(key, _)| key.starts_with(&prefix))
            .filter(move |(key, _)| !key[skip..].contains('/'))
            .map(|(_, resource)| resource)
    }
}

/// What a handler is told about the request it serves.
pub struct ArmContext<'a> {
    pub engine: &'a Engine,
    pub subscription_id: &'a str,
    /// Empty for subscription-level provider actions.
    pub resource_group: &'a str,
    pub api_version: &'a str,
}

/// A `PUT` of one resource.
pub struct PutRequest<'a> {
    /// The resource's canonical id.
    pub id: &'a str,
    pub name: &'a str,
    /// Normalized location the resource will be stored with.
    pub location: Option<&'a str>,
    /// The stored resource when this is an update.
    pub existing: Option<&'a Resource>,
    pub body: &'a Value,
}

/// The behaviour of one resource type. The ARM layer owns the envelope and the
/// store; the handler owns `properties`, validation and actions.
///
/// Handlers are called without the ARM store locked, so they may lock their own
/// service state freely.
pub trait ResourceTypeHandler: Send + Sync {
    /// Canonical type, e.g. `Microsoft.Storage/storageAccounts`.
    fn resource_type(&self) -> &'static str;

    /// Validates a `PUT` and returns the `properties` to store.
    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError>;

    /// Returns the `properties` to store after a `PATCH`. The default merges the
    /// body's `properties` members over the stored ones.
    fn patch(
        &self,
        _ctx: &ArmContext<'_>,
        existing: &Resource,
        body: &Value,
    ) -> Result<Value, ArmError> {
        let mut properties = existing.properties.clone();
        if let (Some(target), Some(changes)) = (
            properties.as_object_mut(),
            body.get("properties").and_then(Value::as_object),
        ) {
            for (key, value) in changes {
                target.insert(key.clone(), value.clone());
            }
        }
        Ok(properties)
    }

    /// Adjusts the envelope (`sku`, `kind`, ...) just before a `PUT` or `PATCH` is
    /// stored, for example to fill in a default SKU.
    fn prepare(&self, _ctx: &ArmContext<'_>, _resource: &mut Resource) {}

    /// Called once, after a new resource has been stored. This is where a type
    /// creates the child resources the service creates along with it.
    fn created(&self, _ctx: &ArmContext<'_>, _resource: &Resource) {}

    /// Adjusts a copy of the resource every time it is returned, for values that
    /// are computed at read time (counts of children, runtime status).
    fn view(&self, _ctx: &ArmContext<'_>, _resource: &mut Resource) {}

    /// Called before a `DELETE` removes the resource; an error refuses it (a
    /// lease or lock the resource is under, for example).
    fn before_delete(&self, _ctx: &ArmContext<'_>, _resource: &Resource) -> Result<(), ArmError> {
        Ok(())
    }

    /// Called after the resource has been removed from the store.
    fn deleted(&self, _ctx: &ArmContext<'_>, _resource: &Resource) {}

    /// The status a successful `DELETE` answers with. Most types answer
    /// `200`; some specifications allow only `202` or `204`.
    fn delete_status(&self) -> StatusCode {
        StatusCode::OK
    }

    /// The actions [`ResourceTypeHandler::action`] implements, as the spec names
    /// them. Used to report coverage; dispatch does not depend on it.
    fn actions(&self) -> &'static [&'static str] {
        &[]
    }

    /// A `POST .../{name}/{action}` on an existing resource.
    fn action(
        &self,
        _ctx: &ArmContext<'_>,
        _resource: &Resource,
        action: &str,
        _body: &Value,
    ) -> Result<Value, ArmError> {
        Err(ArmError::not_implemented(format_args!(
            "the '{action}' action on {}",
            self.resource_type()
        )))
    }
}

/// A subscription-level `POST /subscriptions/{sub}/providers/{Namespace}/{action}`.
pub type ProviderAction =
    Arc<dyn Fn(&ArmContext<'_>, &Value) -> Result<Value, ArmError> + Send + Sync>;

/// The path parameters and body of a request served by an [`OperationHandler`].
pub struct OperationRequest<'a> {
    /// Path parameters by the name the specification gives them, e.g. `vaultName`.
    pub params: &'a HashMap<String, String>,
    pub body: &'a Value,
    pub headers: &'a http::HeaderMap,
}

impl OperationRequest<'_> {
    /// A request header, if present and text.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// A path parameter; empty if the operation's path has no such parameter.
    pub fn param(&self, name: &str) -> &str {
        self.params.get(name).map(String::as_str).unwrap_or("")
    }
}

/// Implements one modeled operation, named by its `operationId`, directly. This is
/// for operations the generic resource routing cannot express: location- or
/// tenant-scoped ones, and resource types whose data lives in a service's own
/// state rather than the ARM store. Returns the status and JSON body; a `Null`
/// body is sent empty.
pub type OperationHandler = Arc<
    dyn Fn(&ArmContext<'_>, &OperationRequest<'_>) -> Result<(StatusCode, Value), ArmError> + Send + Sync,
>;

/// What one modeled operation does, as far as Resource Manager routing goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationKind {
    Put,
    Get,
    Head,
    Patch,
    Delete,
    /// `GET` of a collection inside a resource group (or inside a parent resource).
    List,
    /// `GET` of every resource of the type in the subscription.
    ListInSubscription,
    /// `POST .../{name}/{action}` on a resource.
    Action(&'static str),
    /// `POST /subscriptions/{id}/providers/{Namespace}/{action}`.
    ProviderAction(&'static str),
    /// A shape the ARM layer does not route yet (tenant- or location-scoped, ...).
    Other,
}

/// One operation of a provider's API specification.
#[derive(Debug)]
pub struct ScaffoldedOperation {
    /// The spec's `operationId`, e.g. `StorageAccounts_ListKeys`.
    pub id: &'static str,
    pub method: &'static str,
    pub path: &'static str,
    pub kind: OperationKind,
}

/// One resource type of a provider's API specification.
#[derive(Debug)]
pub struct ScaffoldedType {
    /// Canonical type, e.g. `Microsoft.Storage/storageAccounts/blobServices`.
    pub resource_type: &'static str,
    /// Every `api-version` the type is defined in.
    pub api_versions: &'static [&'static str],
    /// The rules for the type's name and its `PUT` body, as JSON, for
    /// [`ArmRegistry::register_generic`]. `None` when the type has no `PUT`.
    pub schema: Option<&'static str>,
    pub operations: &'static [ScaffoldedOperation],
}

/// A provider's whole modeled surface, generated by `emulator-scaffold` from the
/// provider's OpenAPI documents. Registering it mounts every resource type: the
/// ones without a hand-written handler answer `501` until one is written.
#[derive(Debug)]
pub struct ScaffoldedProvider {
    pub namespace: &'static str,
    pub types: &'static [ScaffoldedType],
    /// Operations that do not belong to a resource type.
    pub provider_operations: &'static [ScaffoldedOperation],
}

/// Whether one modeled operation is implemented by this emulator.
pub struct OperationCoverage {
    pub namespace: &'static str,
    /// `None` for provider-level operations.
    pub resource_type: Option<&'static str>,
    pub operation: &'static ScaffoldedOperation,
    /// Served by behaviour written for the type.
    pub implemented: bool,
    /// Served from the specification's schema alone ([`ArmRegistry::register_generic`]).
    pub generic: bool,
}

/// A child resource type that only Microsoft.Network creates: private endpoint
/// connections (made by creating a private endpoint) and network security
/// perimeter configurations (made by associating a perimeter). The emulator does
/// not model Microsoft.Network, so none can exist: listings are empty, reads are
/// `404`, and a `PUT` — which can only update an existing one — is `404` too.
pub struct CreatedByNetwork(pub &'static str);

impl ResourceTypeHandler for CreatedByNetwork {
    fn resource_type(&self) -> &'static str {
        self.0
    }

    // `reconcile` on a configuration that cannot exist is the generic 404.
    fn actions(&self) -> &'static [&'static str] {
        &["reconcile"]
    }

    fn put(&self, _ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        let kind = self.0.rsplit('/').next().unwrap_or(self.0);
        Err(ArmError::not_found(
            "ResourceNotFound",
            format!(
                "The {kind} resource '{}' was not found. These resources are created by Microsoft.Network, which this emulator does not model, so none exist.",
                request.name
            ),
        ))
    }
}

/// Stands in for a modeled resource type nobody has implemented yet.
struct ScaffoldStub(&'static str);

impl ResourceTypeHandler for ScaffoldStub {
    fn resource_type(&self) -> &'static str {
        self.0
    }

    fn put(&self, _ctx: &ArmContext<'_>, _request: &PutRequest<'_>) -> Result<Value, ArmError> {
        Err(ArmError::not_implemented(format_args!("the resource type {}", self.0)))
    }
}

/// Reads a Key Vault secret's value for a template deployment parameter:
/// `(vault name, secret name, version)`, `None` if there is no such enabled
/// secret. Key Vault registers it; the template engine cannot see its store.
pub type SecretResolver = Arc<dyn Fn(&Engine, &str, &str, Option<&str>) -> Option<String> + Send + Sync>;

/// The resource types and provider actions mounted on this emulator.
#[derive(Default)]
pub struct ArmRegistry {
    secret_resolver: Option<SecretResolver>,
    types: HashMap<String, Arc<dyn ResourceTypeHandler>>,
    actions: HashMap<String, ProviderAction>,
    scaffolds: Vec<&'static ScaffoldedProvider>,
    /// `operationId` → the handler that implements it directly.
    operations: HashMap<&'static str, OperationHandler>,
    /// Lowercased types served by a [`ScaffoldStub`].
    stubs: HashSet<String>,
    /// Lowercased types served by a [`crate::schema::SchemaResource`].
    generic: HashSet<String>,
    /// Lowercased type → the `api-version`s its specification defines.
    api_versions: HashMap<String, &'static [&'static str]>,
}

impl ArmRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_type(&mut self, handler: impl ResourceTypeHandler + 'static) {
        let key = handler.resource_type().to_ascii_lowercase();
        self.stubs.remove(&key);
        self.types.insert(key, Arc::new(handler));
    }

    /// Serves each of `types` from the provider's schema alone
    /// ([`crate::schema::SchemaResource`]). Opt in only for types whose real
    /// behaviour is storing configuration: the generic handler enforces what the
    /// specification says and nothing more. `coverage` reports these as generic,
    /// never as implemented. Call before [`ArmRegistry::register_scaffold`].
    ///
    /// Panics if a type is not in the provider, has no `PUT` schema, or already
    /// has a handler: all three are mistakes in the registration code.
    pub fn register_generic(&mut self, provider: &'static ScaffoldedProvider, types: &[&str]) {
        for wanted in types {
            let kind = provider
                .types
                .iter()
                .find(|kind| kind.resource_type.eq_ignore_ascii_case(wanted))
                .unwrap_or_else(|| panic!("{wanted} is not a type of {}", provider.namespace));
            let schema: Value = kind
                .schema
                .and_then(|text| serde_json::from_str(text).ok())
                .unwrap_or_else(|| panic!("{wanted} has no PUT schema to serve it from"));
            let key = kind.resource_type.to_ascii_lowercase();
            assert!(!self.types.contains_key(&key) || self.stubs.contains(&key), "{wanted} already has a handler");
            self.stubs.remove(&key);
            self.types
                .insert(key.clone(), Arc::new(crate::schema::SchemaResource::new(kind.resource_type, &schema)));
            self.generic.insert(key);
        }
    }

    /// Mounts a provider's modeled surface. Types that already have a handler keep
    /// it and gain `api-version` checking; every other type is mounted as a stub.
    pub fn register_scaffold(&mut self, provider: &'static ScaffoldedProvider) {
        for kind in provider.types {
            let key = kind.resource_type.to_ascii_lowercase();
            self.api_versions.insert(key.clone(), kind.api_versions);
            if !self.types.contains_key(&key) {
                self.types.insert(key.clone(), Arc::new(ScaffoldStub(kind.resource_type)));
                self.stubs.insert(key);
            }
        }
        self.scaffolds.push(provider);
    }

    /// Every modeled operation and whether the emulator implements it.
    pub fn coverage(&self) -> Vec<OperationCoverage> {
        let mut out = Vec::new();
        for provider in &self.scaffolds {
            for kind in provider.types {
                let key = kind.resource_type.to_ascii_lowercase();
                let is_generic = self.generic.contains(&key);
                let handler = self
                    .types
                    .get(&key)
                    .filter(|_| !self.stubs.contains(&key) && !is_generic);
                for operation in kind.operations {
                    let generic = is_generic
                        && !self.operations.contains_key(operation.id)
                        && !matches!(
                            operation.kind,
                            OperationKind::Action(_) | OperationKind::ProviderAction(_) | OperationKind::Other
                        );
                    let implemented = self.operations.contains_key(operation.id)
                        || match (handler, operation.kind) {
                            (None, _) | (_, OperationKind::Other | OperationKind::ProviderAction(_)) => false,
                            (Some(handler), OperationKind::Action(name)) => {
                                handler.actions().iter().any(|a| a.eq_ignore_ascii_case(name))
                            }
                            (Some(_), _) => true,
                        };
                    out.push(OperationCoverage {
                        namespace: provider.namespace,
                        resource_type: Some(kind.resource_type),
                        operation,
                        implemented,
                        generic,
                    });
                }
            }
            for operation in provider.provider_operations {
                let implemented = self.operations.contains_key(operation.id)
                    || match operation.kind {
                        OperationKind::ProviderAction(name) => self
                            .actions
                            .contains_key(&format!("{}/{name}", provider.namespace).to_ascii_lowercase()),
                        _ => false,
                    };
                out.push(OperationCoverage {
                    namespace: provider.namespace,
                    resource_type: None,
                    operation,
                    implemented,
                    generic: false,
                });
            }
        }
        out
    }

    /// Registers `Namespace/actionName`, e.g. `Microsoft.Storage/checkNameAvailability`.
    pub fn register_provider_action(&mut self, path: &str, action: ProviderAction) {
        self.actions.insert(path.to_ascii_lowercase(), action);
    }

    /// Implements the modeled operation `operation_id` with `handler`. The path it
    /// answers on is the one the provider's scaffold gives that operation, so the
    /// scaffold must be registered too. It takes precedence over generic routing.
    /// Lets template deployments resolve Key Vault references in parameters.
    pub fn register_secret_resolver(&mut self, resolver: SecretResolver) {
        self.secret_resolver = Some(resolver);
    }

    pub fn register_operation(&mut self, operation_id: &'static str, handler: OperationHandler) {
        self.operations.insert(operation_id, handler);
    }

    /// The directly implemented operation that `method path` addresses, with the
    /// path parameters it captured.
    fn match_operation(
        &self,
        method: &Method,
        segments: &[String],
    ) -> Option<(&OperationHandler, HashMap<String, String>)> {
        if self.operations.is_empty() {
            return None;
        }
        let all = self.scaffolds.iter().flat_map(|provider| {
            provider
                .types
                .iter()
                .flat_map(|kind| kind.operations.iter())
                .chain(provider.provider_operations.iter())
        });
        for operation in all {
            let Some(handler) = self.operations.get(operation.id) else {
                continue;
            };
            if operation.method != method.as_str() {
                continue;
            }
            let template: Vec<&str> = operation.path.split('/').filter(|s| !s.is_empty()).collect();
            let mut params = HashMap::new();
            // `{scope}` at the start stands for a whole resource id: any number
            // of segments before the rest of the template.
            let (template, segments) = match template.split_first() {
                Some((&"{scope}", rest)) if segments.len() > rest.len() => {
                    let split = segments.len() - rest.len();
                    params.insert("scope".to_string(), format!("/{}", segments[..split].join("/")));
                    (rest.to_vec(), &segments[split..])
                }
                _ => (template, segments),
            };
            if template.len() != segments.len() {
                continue;
            }
            let matched = template.iter().zip(segments).all(|(expected, actual)| {
                match expected.strip_prefix('{').and_then(|e| e.strip_suffix('}')) {
                    Some(name) => {
                        params.insert(name.to_string(), actual.clone());
                        true
                    }
                    None => expected.eq_ignore_ascii_case(actual),
                }
            });
            if matched {
                return Some((handler, params));
            }
        }
        None
    }

    /// Canonical namespace → its registered canonical type names (without namespace).
    fn namespaces(&self) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        out.insert(
            "Microsoft.Resources".into(),
            vec!["deployments".into(), "resourceGroups".into(), "subscriptions".into()],
        );
        out.insert(
            "Microsoft.Authorization".into(),
            vec!["roleAssignments".into(), "roleDefinitions".into()],
        );
        for handler in self.types.values() {
            if let Some((namespace, kind)) = handler.resource_type().split_once('/') {
                out.entry(namespace.into()).or_default().push(kind.into());
            }
        }
        for types in out.values_mut() {
            types.sort();
        }
        out
    }
}

fn is_valid_api_version(version: &str) -> bool {
    let bytes = version.as_bytes();
    let date_ok = bytes.len() >= 10
        && bytes[..10].iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        });
    date_ok && (bytes.len() == 10 || version[10..].starts_with('-'))
}

/// Lowercases a location and removes spaces: `East US` and `eastus` are the same.
fn normalize_location(location: &str) -> String {
    location
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn auth_error(engine: &Engine, failure: AuthFailure) -> Response {
    let config = engine.config();
    let (code, message, description) = match failure {
        AuthFailure::Missing => (
            "AuthenticationFailed",
            "Authentication failed. The 'Authorization' header is missing.",
            "The authentication failed because of missing 'Authorization' header.",
        ),
        AuthFailure::Invalid => (
            "InvalidAuthenticationToken",
            "The access token is invalid.",
            "The access token is invalid.",
        ),
        AuthFailure::WrongAudience => (
            "InvalidAuthenticationTokenAudience",
            "The access token has been obtained for wrong audience or resource.",
            "The access token has been obtained for wrong audience or resource.",
        ),
    };
    let challenge = format!(
        "Bearer authorization_uri=\"{}/{}\", error=\"invalid_token\", error_description=\"{}\"",
        config.management_endpoint(),
        config.tenant_id,
        description
    );
    let mut response = ArmError::new(StatusCode::UNAUTHORIZED, code, message).into_response();
    if let Ok(value) = HeaderValue::from_str(&challenge) {
        response.headers_mut().insert(header::WWW_AUTHENTICATE, value);
    }
    response
}

fn metadata_endpoints(engine: &Engine, api_version: &str) -> Value {
    let config = engine.config();
    let management = config.management_endpoint();
    let port = management.rsplit_once(&config.host).map(|(_, p)| p).unwrap_or("");
    let audiences = json!([
        "https://management.core.windows.net/",
        "https://management.azure.com/"
    ]);
    if api_version < "2020-06-01" {
        return json!({
            "galleryEndpoint": "",
            "graphEndpoint": format!("{management}/"),
            "portalEndpoint": "",
            "authentication": {"loginEndpoint": format!("{management}/"), "audiences": audiences},
        });
    }
    let cloud = json!({
        "name": "AzureEmulator",
        "portal": "",
        "authentication": {
            "loginEndpoint": format!("{management}/"),
            "audiences": audiences,
            // As on Azure: the multi-tenant authority. Clients treat anything
            // else as Azure Stack (Terraform's azurerm provider refuses it).
            "tenant": "common",
            "identityProvider": "AAD",
        },
        "resourceManager": format!("{management}/"),
        "graph": "",
        "graphAudience": "",
        "gallery": "",
        "media": "",
        "batch": "",
        "sqlManagement": "",
        "vmImageAliasDoc": "",
        "activeDirectoryDataLake": "",
        // There is no Microsoft Graph here; this keeps clients that require
        // the field (Terraform's azurerm provider) pointed at the emulator.
        "microsoftGraphResourceId": format!("{management}/"),
        "suffixes": {
            "storage": format!("core.{}{}", config.dns_suffix, port),
            "keyVaultDns": format!("vault.{}{}", config.dns_suffix, port),
            "mhsmDns": format!("managedhsm.{}{}", config.dns_suffix, port),
        },
    });
    // 2022-09-01 and later describe the one cloud this endpoint belongs to, as
    // an object (what the Go SDK behind Terraform's azurerm provider reads);
    // earlier versions list clouds in an array.
    if api_version >= "2022-09-01" {
        cloud
    } else {
        json!([cloud])
    }
}

/// Serves one Resource Manager request.
pub async fn handle(
    engine: Arc<Engine>,
    registry: Arc<ArmRegistry>,
    request: Request<Body>,
) -> Response {
    let (parts, body) = request.into_parts();
    let mut response = match serve(&engine, &registry, &parts, body).await {
        Ok(response) => response,
        Err(error) => error,
    };
    let id = request_id();
    for name in ["x-ms-request-id", "x-ms-correlation-request-id"] {
        if let Ok(value) = HeaderValue::from_str(&id) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

async fn serve(
    engine: &Engine,
    registry: &ArmRegistry,
    parts: &http::request::Parts,
    body: Body,
) -> Result<Response, Response> {
    let config = engine.config();
    let query = parse_query(parts.uri.query());
    let api_version = query
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("api-version"))
        .map(|(_, v)| v.as_str());
    let segments: Vec<String> = parts
        .uri
        .path()
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| percent_decode(s, false))
        .collect();
    let lower: Vec<String> = segments.iter().map(|s| s.to_ascii_lowercase()).collect();
    let lower: Vec<&str> = lower.iter().map(String::as_str).collect();

    // The cloud metadata document is what `az cloud register` and SDK cloud
    // configuration read before they can authenticate, so it is unauthenticated.
    if lower == ["metadata", "endpoints"] && parts.method == Method::GET {
        let body = metadata_endpoints(engine, api_version.unwrap_or("2022-09-01"));
        return Ok(json_response(StatusCode::OK, &body));
    }

    let audiences = [
        "https://management.azure.com".to_string(),
        "https://management.core.windows.net".to_string(),
        config.management_endpoint(),
    ];
    let claims = auth::authorize(&parts.headers, &audiences).map_err(|f| auth_error(engine, f))?;

    let api_version = api_version.ok_or_else(|| {
        ArmError::bad_request(
            "MissingApiVersionParameter",
            "The api-version query parameter (?api-version=) is required for all requests.",
        )
        .into_response()
    })?;
    if !is_valid_api_version(api_version) {
        return Err(ArmError::bad_request(
            "InvalidApiVersionParameter",
            format!("The api-version '{api_version}' is invalid."),
        )
        .into_response());
    }

    let bytes = read_body(body).await?;
    let body: Value = if bytes.iter().all(u8::is_ascii_whitespace) {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).map_err(|_| {
            ArmError::bad_request(
                "InvalidRequestContent",
                "The request content was invalid and could not be deserialized.",
            )
            .into_response()
        })?
    };

    let request = ArmRequest {
        engine,
        registry,
        method: &parts.method,
        headers: &parts.headers,
        api_version,
        body: &body,
        principal: Some(&claims.oid),
        filter: query
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("$filter"))
            .map(|(_, v)| v.clone()),
    };
    request
        .route(&segments, &lower)
        .map_err(IntoResponse::into_response)
}

struct ArmRequest<'a> {
    engine: &'a Engine,
    registry: &'a ArmRegistry,
    method: &'a Method,
    headers: &'a http::HeaderMap,
    api_version: &'a str,
    body: &'a Value,
    /// The caller's object id; checked against role assignments when RBAC is
    /// enforced. Internal requests (a deployment's resources) carry the caller's.
    principal: Option<&'a str>,
    /// The request's `$filter`, for the APIs that take one.
    filter: Option<String>,
}

fn ok(body: Value) -> Result<Response, ArmError> {
    Ok(json_response(StatusCode::OK, &body))
}

fn list(items: Vec<Value>) -> Result<Response, ArmError> {
    ok(json!({"value": items}))
}

impl ArmRequest<'_> {
    fn unsupported(&self, segments: &[String]) -> ArmError {
        ArmError::not_implemented(format_args!("{} /{}", self.method, segments.join("/")))
    }

    fn subscription_json(&self) -> Value {
        let config = self.engine.config();
        json!({
            "id": format!("/subscriptions/{}", config.subscription_id),
            "subscriptionId": config.subscription_id,
            "tenantId": config.tenant_id,
            "displayName": "Azure Emulator",
            "state": "Enabled",
            "authorizationSource": "RoleBased",
            "managedByTenants": [],
        })
    }

    fn provider_json(&self, namespace: &str, types: &[String]) -> Value {
        json!({
            "id": format!("/subscriptions/{}/providers/{namespace}", self.engine.config().subscription_id),
            "namespace": namespace,
            "registrationState": "Registered",
            "registrationPolicy": "RegistrationRequired",
            "resourceTypes": types.iter().map(|t| json!({"resourceType": t})).collect::<Vec<_>>(),
        })
    }

    fn provider(&self, namespace: &str) -> Result<Response, ArmError> {
        self.registry
            .namespaces()
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(namespace))
            .map(|(name, types)| json_response(StatusCode::OK, &self.provider_json(name, types)))
            .ok_or_else(|| invalid_namespace(namespace))
    }

    /// Serves an operation a provider implements directly.
    fn direct_operation(
        &self,
        handler: &OperationHandler,
        params: &HashMap<String, String>,
    ) -> Result<Response, ArmError> {
        if let Some(subscription) = params.get("subscriptionId") {
            if !subscription.eq_ignore_ascii_case(&self.engine.config().subscription_id) {
                return Err(ArmError::not_found(
                    "SubscriptionNotFound",
                    format!("The subscription '{subscription}' could not be found."),
                ));
            }
        }
        let group = match params.get("resourceGroupName") {
            Some(name) => self.require_group(name)?.name,
            None => String::new(),
        };
        let request = OperationRequest {
            params,
            body: self.body,
            headers: self.headers,
        };
        let (status, body) = handler(&self.context(&group), &request)?;
        // An entity tag in the body is also the response's ETag header.
        let etag = body.get("etag").and_then(Value::as_str).map(str::to_string);
        let mut response = match body {
            Value::Null => status.into_response(),
            body => json_response(status, &body),
        };
        if let Some(value) = etag.and_then(|etag| HeaderValue::from_str(&etag).ok()) {
            response.headers_mut().insert(header::ETAG, value);
        }
        // A long-running operation answers 202 with a status URL. Work in the
        // emulator is done before the first response, so that URL reports success.
        if status == StatusCode::ACCEPTED {
            let config = self.engine.config();
            let location = format!(
                "{}/subscriptions/{}/operationresults/{}?api-version={}",
                config.management_endpoint(),
                config.subscription_id,
                request_id(),
                self.api_version
            );
            if let Ok(value) = HeaderValue::from_str(&location) {
                response.headers_mut().insert(header::LOCATION, value);
            }
            response.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("0"));
        }
        Ok(response)
    }

    fn route(&self, segments: &[String], lower: &[&str]) -> Result<Response, ArmError> {
        self.check_access(segments)?;
        // Role assignments and definitions are extension resources: they hang off
        // any scope, `{scope}/providers/Microsoft.Authorization/{kind}/...`.
        if let Some(at) = lower.windows(3).position(|w| {
            w[0] == "providers" && w[1] == "microsoft.authorization" && matches!(w[2], "roleassignments" | "roledefinitions")
        }) {
            let scope = format!("/{}", segments[..at].join("/"));
            return self.authorization(&scope, lower[at + 2], &segments[at + 3..]);
        }
        if let Some((handler, params)) = self.registry.match_operation(self.method, segments) {
            return self.direct_operation(handler, &params);
        }
        let get = self.method == Method::GET;
        match lower {
            ["subscriptions"] if get => list(vec![self.subscription_json()]),
            ["tenants"] if get => {
                let tenant = &self.engine.config().tenant_id;
                list(vec![json!({"id": format!("/tenants/{tenant}"), "tenantId": tenant})])
            }
            ["providers", _] if get => self.provider(&segments[1]),
            ["providers", "microsoft.resources", "calculatetemplatehash"] if self.method == Method::POST => {
                self.calculate_template_hash()
            }
            ["subscriptions", subscription, rest @ ..] => {
                let config = self.engine.config();
                if !subscription.eq_ignore_ascii_case(&config.subscription_id) {
                    return Err(ArmError::not_found(
                        "SubscriptionNotFound",
                        format!("The subscription '{}' could not be found.", segments[1]),
                    ));
                }
                self.route_subscription(&segments[2..], rest)
            }
            _ => Err(self.unsupported(segments)),
        }
    }

    fn route_subscription(&self, segments: &[String], lower: &[&str]) -> Result<Response, ArmError> {
        let get = self.method == Method::GET;
        match lower {
            [] if get => ok(self.subscription_json()),
            ["resourcegroups"] if get => self.list_groups(),
            ["resourcegroups", _] => self.resource_group(&segments[1]),
            ["resourcegroups", _, "providers", "microsoft.resources", "deployments", rest @ ..] => {
                self.deployments(Some(&segments[1]), &segments[5..], rest)
            }
            ["providers", "microsoft.resources", "deployments", rest @ ..] => {
                self.deployments(None, &segments[3..], rest)
            }
            // Deployment operations also answer on the older path without the
            // provider namespace, which is the one the SDKs use.
            ["resourcegroups", _, "deployments", rest @ ..] => self.deployments(Some(&segments[1]), &segments[3..], rest),
            ["deployments", rest @ ..] => self.deployments(None, &segments[1..], rest),
            ["resourcegroups", _, "resources"] if get => {
                let group = self.require_group(&segments[1])?;
                self.list_generic(Some(&group.name))
            }
            ["resources"] if get => self.list_generic(None),
            ["providers"] if get => list(
                self.registry
                    .namespaces()
                    .iter()
                    .map(|(name, types)| self.provider_json(name, types))
                    .collect(),
            ),
            ["providers", _] if get => self.provider(&segments[1]),
            // The status URL a long-running delete hands back. Work in the emulator
            // completes before the first response, so the operation is always done.
            ["operationresults", _] if get => Ok(StatusCode::OK.into_response()),
            ["resourcegroups", _, "providers", _, rest @ ..] if !rest.is_empty() => {
                self.resource_request(&segments[1], &segments[3], &segments[4..])
            }
            ["providers", _, _] if get => self.list_in_subscription(&segments[1], &segments[2]),
            ["providers", namespace, action] if self.method == Method::POST => {
                let handler = self
                    .registry
                    .actions
                    .get(&format!("{namespace}/{action}"))
                    .ok_or_else(|| self.unsupported(segments))?;
                ok(handler(&self.context(""), self.body)?)
            }
            _ => Err(self.unsupported(segments)),
        }
    }

    fn context<'a>(&'a self, resource_group: &'a str) -> ArmContext<'a> {
        ArmContext {
            engine: self.engine,
            subscription_id: &self.engine.config().subscription_id,
            resource_group,
            api_version: self.api_version,
        }
    }

    // ---- resource groups -------------------------------------------------------

    fn group_json(&self, group: &ResourceGroup) -> Value {
        let mut out = json!({
            "id": self.group_id(&group.name),
            "name": group.name,
            "type": "Microsoft.Resources/resourceGroups",
            "location": group.location,
            "properties": {"provisioningState": "Succeeded"},
        });
        if let Some(tags) = &group.tags {
            out["tags"] = tags.clone();
        }
        if let Some(managed_by) = &group.managed_by {
            out["managedBy"] = managed_by.clone();
        }
        out
    }

    fn group_id(&self, name: &str) -> String {
        format!(
            "/subscriptions/{}/resourceGroups/{name}",
            self.engine.config().subscription_id
        )
    }

    fn require_group(&self, name: &str) -> Result<ResourceGroup, ArmError> {
        let state = self.engine.state::<ArmState>();
        let state = state.lock();
        state
            .groups
            .get(&name.to_ascii_lowercase())
            .cloned()
            .ok_or_else(|| group_not_found(name))
    }

    fn list_groups(&self) -> Result<Response, ArmError> {
        let state = self.engine.state::<ArmState>();
        let state = state.lock();
        list(state.groups.values().map(|g| self.group_json(g)).collect())
    }

    fn resource_group(&self, name: &str) -> Result<Response, ArmError> {
        let key = name.to_ascii_lowercase();
        let handle = self.engine.state::<ArmState>();
        match self.method.as_str() {
            "GET" => ok(self.group_json(&self.require_group(name)?)),
            "HEAD" => {
                self.require_group(name)?;
                Ok(StatusCode::NO_CONTENT.into_response())
            }
            "PUT" => {
                let location = self
                    .body
                    .get("location")
                    .and_then(Value::as_str)
                    .map(normalize_location)
                    .ok_or_else(|| {
                        ArmError::bad_request(
                            "LocationRequired",
                            "The location property is required for this definition.",
                        )
                    })?;
                let mut state = handle.lock();
                let existing = state.groups.get(&key);
                if let Some(existing) = existing {
                    if existing.location != location {
                        return Err(ArmError::conflict(
                            "InvalidResourceGroupLocation",
                            format!(
                                "Invalid resource group location '{location}'. The Resource group already exists in location '{}'.",
                                existing.location
                            ),
                        ));
                    }
                }
                let status = if existing.is_some() {
                    StatusCode::OK
                } else {
                    StatusCode::CREATED
                };
                let group = ResourceGroup {
                    // A resource group keeps the casing it was created with.
                    name: existing.map(|g| g.name.clone()).unwrap_or_else(|| name.to_string()),
                    location,
                    tags: self.body.get("tags").filter(|t| !t.is_null()).cloned(),
                    managed_by: self.body.get("managedBy").filter(|t| !t.is_null()).cloned(),
                };
                let body = self.group_json(&group);
                state.groups.insert(key, group);
                Ok(json_response(status, &body))
            }
            "PATCH" => {
                let mut state = handle.lock();
                let group = state.groups.get_mut(&key).ok_or_else(|| group_not_found(name))?;
                if let Some(tags) = self.body.get("tags") {
                    group.tags = Some(tags.clone()).filter(|t| !t.is_null());
                }
                ok(self.group_json(group))
            }
            "DELETE" => {
                let removed: Vec<Resource> = {
                    let mut state = handle.lock();
                    let group = state.groups.remove(&key).ok_or_else(|| group_not_found(name))?;
                    let prefix = format!("{}/", self.group_id(&group.name).to_ascii_lowercase());
                    let ids: Vec<String> = state
                        .resources
                        .range(prefix.clone()..)
                        .take_while(|(id, _)| id.starts_with(&prefix))
                        .map(|(id, _)| id.clone())
                        .collect();
                    ids.iter().filter_map(|id| state.resources.remove(id)).collect()
                };
                self.notify_deleted(name, &removed);
                deployments::forget_group(self.engine, &self.group_id(name));
                authorization::forget_scope(self.engine, &self.group_id(name));
                let location = format!(
                    "{}/subscriptions/{}/operationresults/{}?api-version={}",
                    self.engine.config().management_endpoint(),
                    self.engine.config().subscription_id,
                    request_id(),
                    self.api_version
                );
                Ok((StatusCode::ACCEPTED, [(header::LOCATION, location)]).into_response())
            }
            _ => Err(self.unsupported(&["resourceGroups".into(), name.into()])),
        }
    }

    fn notify_deleted(&self, resource_group: &str, removed: &[Resource]) {
        let ctx = self.context(resource_group);
        for resource in removed {
            if let Some(handler) = self.registry.types.get(&resource.resource_type.to_ascii_lowercase()) {
                handler.deleted(&ctx, resource);
            }
        }
    }

    // ---- generic resources -----------------------------------------------------

    fn list_generic(&self, resource_group: Option<&str>) -> Result<Response, ArmError> {
        let prefix = match resource_group {
            Some(group) => format!("{}/", self.group_id(group).to_ascii_lowercase()),
            None => String::new(),
        };
        let filter = ResourceFilter::parse(self.filter.as_deref())?;
        let state = self.engine.state::<ArmState>();
        let state = state.lock();
        let items: Vec<Value> = state
            .resources
            .iter()
            .filter(|(id, _)| id.starts_with(&prefix))
            // The generic listing shows top-level resources, not their children.
            .filter(|(_, r)| r.resource_type.matches('/').count() == 1)
            .filter(|(_, r)| filter.matches(r))
            .map(|(_, r)| r.summary())
            .collect();
        list(items)
    }

    fn handler(&self, namespace: &str, types: &[&str]) -> Result<&Arc<dyn ResourceTypeHandler>, ArmError> {
        let full = format!("{namespace}/{}", types.join("/")).to_ascii_lowercase();
        if let Some(versions) = self.registry.api_versions.get(&full) {
            if !versions.iter().any(|v| v.eq_ignore_ascii_case(self.api_version)) {
                // The real message also names the location and supported locations.
                return Err(ArmError::bad_request(
                    "NoRegisteredProviderFound",
                    format!(
                        "No registered resource provider found for API version '{}' for type '{}'. The supported api-versions are '{}'.",
                        self.api_version,
                        types.join("/"),
                        versions.join(", ")
                    ),
                ));
            }
        }
        self.registry.types.get(&full).ok_or_else(|| {
            let prefix = format!("{}/", namespace.to_ascii_lowercase());
            if self.registry.types.keys().any(|k| k.starts_with(&prefix)) {
                ArmError::bad_request(
                    "InvalidResourceType",
                    format!(
                        "The resource type '{}' could not be found in the namespace '{namespace}' for api version '{}'.",
                        types.join("/"),
                        self.api_version
                    ),
                )
            } else {
                invalid_namespace(namespace)
            }
        })
    }

    fn list_in_subscription(&self, namespace: &str, kind: &str) -> Result<Response, ArmError> {
        let handler = self.handler(namespace, &[kind])?;
        let wanted = handler.resource_type().to_ascii_lowercase();
        let found: Vec<Resource> = {
            let state = self.engine.state::<ArmState>();
            let state = state.lock();
            state
                .resources
                .values()
                .filter(|r| r.resource_type.to_ascii_lowercase() == wanted)
                .cloned()
                .collect()
        };
        let ctx = self.context("");
        list(found.into_iter().map(|r| render(handler.as_ref(), &ctx, r)).collect())
    }

    /// A request under `.../resourceGroups/{rg}/providers/{namespace}/{rest...}`.
    /// `rest` alternates type and name segments, optionally ending in a collection
    /// (`GET`) or an action (`POST`).
    fn resource_request(
        &self,
        group_name: &str,
        namespace: &str,
        rest: &[String],
    ) -> Result<Response, ArmError> {
        let is_action = self.method == Method::POST && rest.len() % 2 == 1 && rest.len() >= 3;
        let (path, action) = match is_action {
            true => (&rest[..rest.len() - 1], Some(rest[rest.len() - 1].as_str())),
            false => (rest, None),
        };
        let types: Vec<&str> = path.iter().step_by(2).map(String::as_str).collect();
        let names: Vec<&str> = path.iter().skip(1).step_by(2).map(String::as_str).collect();
        let handler = self.handler(namespace, &types)?;
        let group = self.require_group(group_name)?;
        let ctx = self.context(&group.name);

        // Canonical id: stored group casing, the handler's type casing, given names.
        let canonical_types: Vec<&str> = handler.resource_type().split('/').collect();
        let kinds = &canonical_types[1..];
        let mut id = format!("{}/providers/{}", self.group_id(&group.name), canonical_types[0]);
        let mut parent_id = None;
        for (index, kind) in kinds.iter().enumerate() {
            if index > 0 && index == kinds.len() - 1 {
                parent_id = Some(id.clone());
            }
            match names.get(index) {
                Some(name) => id.push_str(&format!("/{kind}/{name}")),
                // A trailing type segment with no name: a collection.
                None => id.push_str(&format!("/{kind}")),
            }
        }
        let handle = self.engine.state::<ArmState>();

        if let Some(parent) = &parent_id {
            if handle.lock().resource(parent).is_none() {
                return Err(ArmError::not_found(
                    "ParentResourceNotFound",
                    format!(
                        "Can not perform requested operation on nested resource. Parent resource '{}' not found.",
                        names[..kinds.len() - 1].join("/")
                    ),
                ));
            }
        }

        if names.len() < types.len() {
            if self.method != Method::GET {
                return Err(self.unsupported(rest));
            }
            // Direct children only, copied out so handlers run without the lock.
            let found: Vec<Resource> = handle.lock().children(&id).cloned().collect();
            return list(found.into_iter().map(|r| render(handler.as_ref(), &ctx, r)).collect());
        }

        let name = *names.last().expect("a resource path has a name");
        let key = id.to_ascii_lowercase();
        let existing = handle.lock().resources.get(&key).cloned();
        let not_found = || {
            ArmError::not_found(
                "ResourceNotFound",
                format!(
                    "The Resource '{}/{}' under resource group '{}' was not found. For more details please go to https://aka.ms/ARMResourceNotFoundFix",
                    handler.resource_type(),
                    names.join("/"),
                    group.name
                ),
            )
        };

        if let Some(action) = action {
            let resource = existing.ok_or_else(not_found)?;
            // An action with nothing to return answers with an empty body.
            return match handler.action(&ctx, &resource, action, self.body)? {
                Value::Null => Ok(StatusCode::OK.into_response()),
                body => ok(body),
            };
        }

        match self.method.as_str() {
            "GET" => ok(render(handler.as_ref(), &ctx, existing.ok_or_else(not_found)?)),
            "HEAD" => {
                existing.ok_or_else(not_found)?;
                Ok(StatusCode::NO_CONTENT.into_response())
            }
            "PUT" => {
                // Some types take no body at all; an absent one is an empty object.
                let empty = Map::new();
                let object = match self.body {
                    Value::Null => &empty,
                    body => body.as_object().ok_or_else(|| {
                        ArmError::bad_request(
                            "InvalidRequestContent",
                            "The request content was invalid and could not be deserialized.",
                        )
                    })?,
                };
                let requested = object.get("location").and_then(Value::as_str).map(normalize_location);
                let stored = existing.as_ref().and_then(|r| r.location.clone());
                let location = match (requested, stored) {
                    (Some(new), Some(old)) if new != old => {
                        return Err(ArmError::conflict(
                            "InvalidResourceLocation",
                            format!(
                                "The resource '{name}' already exists in location '{old}' in resource group '{}'. A resource with the same name cannot be created in location '{new}'. Please select a new resource name.",
                                group.name
                            ),
                        ));
                    }
                    (new, old) => new.or(old),
                };
                if location.is_none() && canonical_types.len() == 2 {
                    return Err(ArmError::bad_request(
                        "LocationRequired",
                        "The location property is required for this definition.",
                    ));
                }
                // Checked before the type's handler, which may have side effects.
                let identity = crate::identity::resolve(
                    &mut handle.lock(),
                    object.get("identity"),
                    existing.as_ref().and_then(|r| r.identity.as_ref()),
                    &self.engine.config().tenant_id,
                    false,
                )?;
                let properties = handler.put(
                    &ctx,
                    &PutRequest {
                        id: existing.as_ref().map(|r| r.id.as_str()).unwrap_or(&id),
                        name,
                        location: location.as_deref(),
                        existing: existing.as_ref(),
                        body: self.body,
                    },
                )?;
                let member = |key: &str| object.get(key).filter(|v| !v.is_null()).cloned();
                let is_new = existing.is_none();
                let mut resource = Resource {
                    id: existing.as_ref().map(|r| r.id.clone()).unwrap_or(id),
                    name: existing.as_ref().map(|r| r.name.clone()).unwrap_or_else(|| name.into()),
                    resource_type: handler.resource_type().to_string(),
                    location,
                    tags: member("tags"),
                    kind: member("kind").or_else(|| existing.as_ref().and_then(|r| r.kind.clone())),
                    sku: member("sku"),
                    identity,
                    properties,
                };
                handler.prepare(&ctx, &mut resource);
                handle.lock().resources.insert(key, resource.clone());
                if is_new {
                    handler.created(&ctx, &resource);
                }
                ok(render(handler.as_ref(), &ctx, resource))
            }
            "PATCH" => {
                let mut resource = existing.ok_or_else(not_found)?;
                let empty = Map::new();
                let object = self.body.as_object().unwrap_or(&empty);
                resource.identity = crate::identity::resolve(
                    &mut handle.lock(),
                    object.get("identity"),
                    resource.identity.as_ref(),
                    &self.engine.config().tenant_id,
                    true,
                )?;
                resource.properties = handler.patch(&ctx, &resource, self.body)?;
                for (member, slot) in [("tags", &mut resource.tags), ("sku", &mut resource.sku)] {
                    if let Some(value) = object.get(member) {
                        *slot = Some(value.clone()).filter(|v| !v.is_null());
                    }
                }
                handler.prepare(&ctx, &mut resource);
                handle.lock().resources.insert(key, resource.clone());
                ok(render(handler.as_ref(), &ctx, resource))
            }
            "DELETE" => {
                let Some(target) = &existing else {
                    return Ok(StatusCode::NO_CONTENT.into_response());
                };
                handler.before_delete(&ctx, target)?;
                let removed: Vec<Resource> = {
                    let mut state = handle.lock();
                    let prefix = format!("{key}/");
                    let mut ids: Vec<String> = state
                        .resources
                        .range(prefix.clone()..)
                        .take_while(|(id, _)| id.starts_with(&prefix))
                        .map(|(id, _)| id.clone())
                        .collect();
                    ids.push(key);
                    ids.iter().filter_map(|id| state.resources.remove(id)).collect()
                };
                self.notify_deleted(&group.name, &removed);
                if let Some(resource) = removed.iter().find(|r| r.id.eq_ignore_ascii_case(&target.id)) {
                    authorization::forget_scope(self.engine, &resource.id);
                }
                Ok(handler.delete_status().into_response())
            }
            _ => Err(self.unsupported(rest)),
        }
    }
}

/// The `$filter` of a generic resource listing:
/// `resourceType eq '...'`, `name eq '...'`, `location eq '...'`,
/// `resourceGroup eq '...'`, `substringof('...', name)`, `tagName eq '...'` and
/// `tagValue eq '...'` (together: a tag with that value), combined with `and`
/// and `or`.
#[derive(Default)]
struct ResourceFilter {
    /// Alternatives (`or`), each a list of clauses that must all hold (`and`).
    groups: Vec<Vec<(String, String)>>,
}

impl ResourceFilter {
    fn parse(filter: Option<&str>) -> Result<Self, ArmError> {
        let mut out = Self::default();
        let Some(raw) = filter.map(str::trim).filter(|f| !f.is_empty()) else {
            return Ok(out);
        };
        // Unverified against Azure: the code for a filter it cannot parse.
        let invalid = || {
            ArmError::bad_request(
                "InvalidFilterInQueryString",
                format!("Invalid $filter '{raw}' specified in the query string."),
            )
        };
        let unquote = |text: &str| -> Result<String, ArmError> {
            let text = text.trim();
            text.strip_prefix('\'')
                .and_then(|t| t.strip_suffix('\''))
                .map(|t| t.replace("''", "'"))
                .ok_or_else(invalid)
        };
        for alternative in split_keyword(raw, " or ") {
            let mut clauses = Vec::new();
            for clause in split_keyword(&alternative, " and ") {
                let clause = clause.trim().trim_start_matches('(').trim_end_matches(')').trim();
                let lower = clause.to_ascii_lowercase();
                if let Some(inner) = lower.strip_prefix("substringof(") {
                    let original = &clause[clause.len() - inner.len()..];
                    let (needle, field) = original.rsplit_once(',').ok_or_else(invalid)?;
                    if !field.trim().eq_ignore_ascii_case("name") {
                        return Err(invalid());
                    }
                    clauses.push(("substringof".to_string(), unquote(needle)?));
                    continue;
                }
                let (field, value) = clause.split_once(" eq ").ok_or_else(invalid)?;
                let field = field.trim().to_ascii_lowercase();
                if !matches!(field.as_str(), "resourcetype" | "name" | "location" | "resourcegroup" | "tagname" | "tagvalue") {
                    return Err(invalid());
                }
                clauses.push((field, unquote(value)?));
            }
            out.groups.push(clauses);
        }
        Ok(out)
    }

    fn matches(&self, resource: &Resource) -> bool {
        if self.groups.is_empty() {
            return true;
        }
        self.groups.iter().any(|clauses| {
            let tag_name = clauses.iter().find(|(f, _)| f == "tagname").map(|(_, v)| v.as_str());
            let tags = resource.tags.as_ref().and_then(Value::as_object);
            clauses.iter().all(|(field, value)| match field.as_str() {
                "resourcetype" => resource.resource_type.eq_ignore_ascii_case(value),
                "name" => resource.name.eq_ignore_ascii_case(value),
                "substringof" => resource.name.to_ascii_lowercase().contains(&value.to_ascii_lowercase()),
                "location" => resource.location.as_deref().is_some_and(|l| l.eq_ignore_ascii_case(value)),
                "resourcegroup" => resource
                    .id
                    .split('/')
                    .nth(4)
                    .is_some_and(|group| group.eq_ignore_ascii_case(value)),
                "tagname" => tags.is_some_and(|t| t.keys().any(|k| k.eq_ignore_ascii_case(value))),
                "tagvalue" => tags.is_some_and(|t| {
                    t.iter().any(|(k, v)| {
                        tag_name.is_none_or(|name| k.eq_ignore_ascii_case(name)) && v.as_str() == Some(value.as_str())
                    })
                }),
                _ => false,
            })
        })
    }
}

/// Splits on a keyword (` and `, ` or `) outside quotes, ignoring case.
fn split_keyword(text: &str, keyword: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let chars: Vec<char> = text.chars().collect();
    let keyword: Vec<char> = keyword.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\'' {
            quoted = !quoted;
        }
        let here: String = chars[i..chars.len().min(i + keyword.len())].iter().collect();
        if !quoted && here.eq_ignore_ascii_case(&keyword.iter().collect::<String>()) {
            parts.push(std::mem::take(&mut current));
            i += keyword.len();
            continue;
        }
        current.push(chars[i]);
        i += 1;
    }
    parts.push(current);
    parts
}

/// The resource as its handler presents it.
fn render(handler: &dyn ResourceTypeHandler, ctx: &ArmContext<'_>, mut resource: Resource) -> Value {
    handler.view(ctx, &mut resource);
    resource.to_json()
}

fn group_not_found(name: &str) -> ArmError {
    ArmError::not_found(
        "ResourceGroupNotFound",
        format!("Resource group '{name}' could not be found."),
    )
}

fn invalid_namespace(namespace: &str) -> ArmError {
    ArmError::not_found(
        "InvalidResourceNamespace",
        format!("The resource namespace '{namespace}' is invalid."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_version_shape() {
        assert!(is_valid_api_version("2023-01-01"));
        assert!(is_valid_api_version("2023-01-01-preview"));
        assert!(!is_valid_api_version("2023-1-1"));
        assert!(!is_valid_api_version("latest"));
        assert!(!is_valid_api_version("2023-01-01x"));
    }

    #[test]
    fn locations_normalize() {
        assert_eq!(normalize_location("East US 2"), "eastus2");
    }
}
