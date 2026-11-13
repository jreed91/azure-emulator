//! `Microsoft.Resources/deployments`: template deployments at resource-group and
//! subscription scope, with validation, what-if, operations and export.
//!
//! The work runs before the first response, so a deployment answers `201` (or
//! `200`) already `Succeeded` or `Failed`; clients that poll find it finished.

mod expr;
mod functions;
mod template;

use std::collections::BTreeMap;
use std::time::Instant;

use axum::response::{IntoResponse, Response};
use http::{header, HeaderMap, Method, StatusCode};
use serde_json::{json, Value};

use super::ArmRequest;
use crate::http_util::{json_response, request_id};
use crate::{ArmError, Config};

pub use template::Dispatch;

const DEPLOYMENTS_TYPE: &str = "Microsoft.Resources/deployments";

/// Every stored deployment, keyed by lowercased id.
#[derive(Default)]
pub struct DeploymentState {
    deployments: BTreeMap<String, Record>,
}

impl DeploymentState {
    /// Forgets the deployments of a deleted resource group.
    pub fn remove_group(&mut self, group_id: &str) {
        let prefix = format!("{}/", group_id.to_ascii_lowercase());
        self.deployments.retain(|id, _| !id.starts_with(&prefix));
    }
}

#[derive(Clone)]
struct Record {
    id: String,
    name: String,
    location: Option<String>,
    tags: Option<Value>,
    properties: Value,
    template: Value,
    operations: Vec<Value>,
}

impl Record {
    fn to_json(&self) -> Value {
        let mut out = json!({
            "id": self.id,
            "name": self.name,
            "type": DEPLOYMENTS_TYPE,
            "properties": self.properties,
        });
        if let Some(location) = &self.location {
            out["location"] = json!(location);
        }
        if let Some(tags) = &self.tags {
            out["tags"] = tags.clone();
        }
        out
    }
}

/// The `templateHash` of a template: a hash of its compact JSON.
pub fn template_hash(template: &Value) -> String {
    functions::murmur64(template.to_string().as_bytes()).to_string()
}

/// Waits for a future that is already complete (a response body held in memory).
fn ready<F: std::future::Future>(future: F) -> Option<F::Output> {
    let mut future = std::pin::pin!(future);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(value) => Some(value),
        std::task::Poll::Pending => None,
    }
}

impl Dispatch for ArmRequest<'_> {
    fn call(&self, method: Method, path: &str, api_version: &str, body: &Value) -> Result<(StatusCode, Value), ArmError> {
        let headers = HeaderMap::new();
        let segments: Vec<String> = path.split('/').filter(|s| !s.is_empty()).map(String::from).collect();
        let lower: Vec<String> = segments.iter().map(|s| s.to_ascii_lowercase()).collect();
        let lower: Vec<&str> = lower.iter().map(String::as_str).collect();
        let request = ArmRequest {
            engine: self.engine,
            registry: self.registry,
            method: &method,
            headers: &headers,
            api_version,
            body,
            principal: self.principal,
            filter: None,
        };
        let response = request.route(&segments, &lower)?;
        let status = response.status();
        let bytes = ready(axum::body::to_bytes(response.into_body(), usize::MAX))
            .and_then(Result::ok)
            .unwrap_or_default();
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if status.is_client_error() || status.is_server_error() {
            let code = value.pointer("/error/code").and_then(Value::as_str).unwrap_or("Error");
            let message = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("{method} {path} answered {status}."));
            return Err(ArmError::new(status, code, message));
        }
        Ok((status, value))
    }

    fn config(&self) -> &Config {
        self.engine.config()
    }

    fn environment(&self) -> Value {
        let config = self.engine.config();
        let management = config.management_endpoint();
        let port = management.rsplit_once(&config.host).map(|(_, p)| p).unwrap_or("");
        json!({
            "name": "AzureEmulator",
            "gallery": "",
            "graph": format!("{management}/"),
            "portal": "",
            "graphAudience": format!("{management}/"),
            "activeDirectoryDataLake": "",
            "batch": "",
            "media": "",
            "sqlManagement": "",
            "vmImageAliasDoc": "",
            "resourceManager": format!("{management}/"),
            "authentication": {
                "loginEndpoint": format!("{management}/"),
                "audiences": ["https://management.core.windows.net/", "https://management.azure.com/"],
                "tenant": config.tenant_id,
                "identityProvider": "AAD",
            },
            "suffixes": {
                "storage": format!("core.{}{port}", config.dns_suffix),
                "keyvaultDns": format!(".vault.{}{port}", config.dns_suffix),
                "sqlServerHostname": format!(".database.{}", config.dns_suffix),
                "acrLoginServer": format!(".azurecr.{}", config.dns_suffix),
                "azureFrontDoorEndpointSuffix": "",
                "azureDatalakeStoreFileSystem": "",
                "azureDatalakeAnalyticsCatalogAndJob": "",
            },
        })
    }

    fn api_version_for(&self, resource_type: &str) -> Option<String> {
        let versions = self.registry.api_versions.get(&resource_type.to_ascii_lowercase())?;
        versions.iter().filter(|v| !v.contains("preview")).max().or(versions.iter().max()).map(|v| v.to_string())
    }
}

/// Where a deployment lives.
struct Scope {
    /// The resource group's stored name, or `None` at subscription scope.
    group: Option<String>,
    base: String,
}

fn bad_content(message: impl Into<String>) -> ArmError {
    ArmError::bad_request("InvalidRequestContent", message)
}

fn not_found(name: &str, scope: &Scope) -> ArmError {
    let message = match &scope.group {
        Some(group) => format!("Deployment '{name}' could not be found in resource group '{group}'."),
        None => format!("Deployment '{name}' could not be found."),
    };
    ArmError::not_found("DeploymentNotFound", message)
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.chars().count())
        && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '(' | ')'))
}

impl ArmRequest<'_> {
    /// A request under `{scope}/providers/Microsoft.Resources/deployments`.
    pub(super) fn deployments(&self, group: Option<&str>, rest: &[String], lower: &[&str]) -> Result<Response, ArmError> {
        let subscription = &self.engine.config().subscription_id;
        let scope = match group {
            Some(name) => {
                let group = self.require_group(name)?;
                Scope {
                    base: format!("/subscriptions/{subscription}/resourceGroups/{}", group.name),
                    group: Some(group.name),
                }
            }
            None => Scope {
                group: None,
                base: format!("/subscriptions/{subscription}"),
            },
        };
        let method = self.method.as_str();
        match (method, lower) {
            ("GET", []) => self.list_deployments(&scope),
            (_, [_]) => self.deployment(&scope, &rest[0]),
            ("POST", [_, "validate"]) => self.validate_deployment(&scope, &rest[0]),
            ("POST", [_, "whatif"]) => self.what_if(&scope, &rest[0]),
            ("POST", [_, "exporttemplate"]) => {
                let record = self.find_deployment(&scope, &rest[0])?;
                ok(json!({"template": record.template}))
            }
            ("POST", [_, "cancel"]) => {
                let record = self.find_deployment(&scope, &rest[0])?;
                let state = record.properties["provisioningState"].as_str().unwrap_or("");
                Err(ArmError::conflict(
                    "DeploymentCannotBeCancelled",
                    format!("The deployment '{}' cannot be cancelled because it is in '{state}' state.", record.name),
                ))
            }
            // The status URL a deployment's `Azure-AsyncOperation` header names.
            ("GET", [_, "operationstatuses", _]) => {
                let record = self.find_deployment(&scope, &rest[0])?;
                let mut status = json!({"status": record.properties["provisioningState"]});
                if let Some(error) = record.properties.get("error") {
                    status["error"] = error.clone();
                }
                ok(status)
            }
            ("GET", [_, "operations"]) => {
                let record = self.find_deployment(&scope, &rest[0])?;
                ok(json!({"value": record.operations}))
            }
            ("GET", [_, "operations", _]) => {
                let record = self.find_deployment(&scope, &rest[0])?;
                record
                    .operations
                    .iter()
                    .find(|o| o["operationId"].as_str().is_some_and(|id| id.eq_ignore_ascii_case(&rest[2])))
                    .cloned()
                    .map(ok)
                    .unwrap_or_else(|| {
                        Err(ArmError::not_found(
                            "DeploymentOperationNotFound",
                            format!("Deployment operation '{}' could not be found.", rest[2]),
                        ))
                    })
            }
            _ => Err(self.unsupported(rest)),
        }
    }

    fn deployment_id(&self, scope: &Scope, name: &str) -> String {
        format!("{}/providers/{DEPLOYMENTS_TYPE}/{name}", scope.base)
    }

    fn find_deployment(&self, scope: &Scope, name: &str) -> Result<Record, ArmError> {
        let key = self.deployment_id(scope, name).to_ascii_lowercase();
        let state = self.engine.state::<DeploymentState>();
        let state = state.lock();
        state.deployments.get(&key).cloned().ok_or_else(|| not_found(name, scope))
    }

    fn list_deployments(&self, scope: &Scope) -> Result<Response, ArmError> {
        let prefix = format!("{}/providers/{DEPLOYMENTS_TYPE}/", scope.base).to_ascii_lowercase();
        let state = self.engine.state::<DeploymentState>();
        let state = state.lock();
        let items: Vec<Value> = state
            .deployments
            .iter()
            .filter(|(id, _)| id.starts_with(&prefix) && !id[prefix.len()..].contains('/'))
            .map(|(_, record)| record.to_json())
            .collect();
        ok(json!({"value": items}))
    }

    fn deployment(&self, scope: &Scope, name: &str) -> Result<Response, ArmError> {
        match self.method.as_str() {
            "GET" => ok(self.find_deployment(scope, name)?.to_json()),
            "HEAD" => {
                self.find_deployment(scope, name)?;
                Ok(StatusCode::NO_CONTENT.into_response())
            }
            "DELETE" => {
                let key = self.deployment_id(scope, name).to_ascii_lowercase();
                let removed = self.engine.state::<DeploymentState>().lock().deployments.remove(&key);
                if removed.is_none() {
                    return Ok(StatusCode::NO_CONTENT.into_response());
                }
                let config = self.engine.config();
                let location = format!(
                    "{}/subscriptions/{}/operationresults/{}?api-version={}",
                    config.management_endpoint(),
                    config.subscription_id,
                    request_id(),
                    self.api_version
                );
                Ok((StatusCode::ACCEPTED, [(header::LOCATION, location)]).into_response())
            }
            "PUT" => self.put_deployment(scope, name),
            _ => Err(self.unsupported(&[name.to_string()])),
        }
    }

    /// Reads a deployment request body into the engine's settings.
    fn deployment_request<'s>(&'s self, scope: &Scope, name: &str) -> Result<template::Request<'s>, ArmError> {
        if !valid_name(name) {
            return Err(ArmError::bad_request(
                "InvalidDeploymentName",
                format!("The deployment name '{name}' is invalid. It can only contain letters, digits, '-', '.', '_', '(' and ')', and be at most 64 characters long."),
            ));
        }
        let body = self.body.as_object().ok_or_else(|| bad_content("The request content was invalid and could not be deserialized."))?;
        let properties = body
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(|| bad_content("The request content was invalid and could not be deserialized: 'Required property 'properties' not found in JSON.'"))?;
        let mode = properties
            .get("mode")
            .and_then(Value::as_str)
            .ok_or_else(|| bad_content("The request content was invalid and could not be deserialized: 'Required property 'mode' not found in JSON.'"))?;
        let complete = match mode.to_ascii_lowercase().as_str() {
            "incremental" => false,
            "complete" if scope.group.is_some() => true,
            "complete" => {
                return Err(ArmError::bad_request(
                    "InvalidDeploymentMode",
                    "The deployment mode 'Complete' is only supported for deployments at resource group scope.",
                ))
            }
            _ => return Err(bad_content(format!("The deployment mode '{mode}' is not valid; it must be 'Incremental' or 'Complete'."))),
        };
        if properties.get("templateLink").is_some_and(|v| !v.is_null()) {
            return Err(ArmError::not_implemented("deployments from a templateLink (use an inline template)"));
        }
        if properties.get("parametersLink").is_some_and(|v| !v.is_null()) {
            return Err(ArmError::not_implemented("deployments with a parametersLink (use inline parameters)"));
        }
        let template = properties.get("template").filter(|t| !t.is_null()).cloned().ok_or_else(|| {
            ArmError::bad_request("InvalidDeployment", "The deployment must have either a 'template' or a 'templateLink'.")
        })?;
        let mut parameters = properties.get("parameters").and_then(Value::as_object).cloned().unwrap_or_default();
        // A whole parameters file is accepted too.
        if parameters.contains_key("$schema") {
            parameters = parameters.get("parameters").and_then(Value::as_object).cloned().unwrap_or_default();
        }
        let location = body.get("location").and_then(Value::as_str).map(|l| l.to_string());
        if scope.group.is_none() && location.is_none() {
            return Err(ArmError::bad_request(
                "LocationRequired",
                "The location property is required for this definition: a subscription deployment must specify a location.",
            ));
        }
        Ok(template::Request {
            dispatch: self,
            subscription_id: self.engine.config().subscription_id.clone(),
            resource_group: scope.group.clone(),
            name: name.to_string(),
            id: self.deployment_id(scope, name),
            location,
            template,
            parameters,
            complete,
        })
    }

    fn put_deployment(&self, scope: &Scope, name: &str) -> Result<Response, ArmError> {
        let started = Instant::now();
        let request = self.deployment_request(scope, name)?;
        let template = request.template.clone();
        let location = request.location.clone();
        let complete = request.complete;
        let id = request.id.clone();
        let prepared = template::prepare(request)?;
        let outcome = prepared.execute();

        let mut properties = json!({
            "templateHash": template_hash(&template),
            "parameters": prepared.parameters_view(),
            "mode": if complete { "Complete" } else { "Incremental" },
            "provisioningState": if outcome.error.is_some() { "Failed" } else { "Succeeded" },
            "timestamp": template::timestamp(&chrono::Utc::now()),
            "duration": format!("PT{:.7}S", started.elapsed().as_secs_f64()),
            "correlationId": uuid::Uuid::new_v4().to_string(),
            "providers": prepared.providers_view(),
            "dependencies": prepared.dependencies_view(),
            "outputResources": outcome.output_resources.iter().map(|id| json!({"id": id})).collect::<Vec<_>>(),
        });
        if let Some(outputs) = outcome.outputs {
            properties["outputs"] = outputs;
        }
        if let Some(error) = outcome.error {
            properties["error"] = error;
        }
        if let Some(debug) = self.body.pointer("/properties/debugSetting") {
            properties["debugSetting"] = debug.clone();
        }
        let record = Record {
            id: id.clone(),
            name: name.to_string(),
            location,
            tags: self.body.get("tags").filter(|t| !t.is_null()).cloned(),
            properties,
            template,
            operations: outcome.operations,
        };
        let body = record.to_json();
        let previous = self
            .engine
            .state::<DeploymentState>()
            .lock()
            .deployments
            .insert(id.to_ascii_lowercase(), record);
        let status = if previous.is_some() { StatusCode::OK } else { StatusCode::CREATED };
        let mut response = json_response(status, &body);
        // Clients poll this for the outcome, as they would while Azure works.
        let status_url = format!(
            "{}{id}/operationStatuses/{}?api-version={}",
            self.engine.config().management_endpoint(),
            body["properties"]["correlationId"].as_str().unwrap_or_default(),
            self.api_version
        );
        if let Ok(value) = header::HeaderValue::from_str(&status_url) {
            response.headers_mut().insert("azure-asyncoperation", value);
        }
        Ok(response)
    }

    fn validate_deployment(&self, scope: &Scope, name: &str) -> Result<Response, ArmError> {
        let request = self.deployment_request(scope, name)?;
        let template = request.template.clone();
        let complete = request.complete;
        let id = request.id.clone();
        let location = request.location.clone();
        let prepared = template::prepare(request)?;
        let mut out = json!({
            "id": id,
            "name": name,
            "type": DEPLOYMENTS_TYPE,
            "properties": {
                "templateHash": template_hash(&template),
                "parameters": prepared.parameters_view(),
                "mode": if complete { "Complete" } else { "Incremental" },
                "provisioningState": "Succeeded",
                "timestamp": template::timestamp(&chrono::Utc::now()),
                "duration": "PT0S",
                "correlationId": uuid::Uuid::new_v4().to_string(),
                "providers": prepared.providers_view(),
                "dependencies": prepared.dependencies_view(),
                "validatedResources": prepared.validated_resources(),
            },
        });
        if let Some(location) = location {
            out["location"] = json!(location);
        }
        ok(out)
    }

    fn what_if(&self, scope: &Scope, name: &str) -> Result<Response, ArmError> {
        let request = self.deployment_request(scope, name)?;
        let prepared = template::prepare(request)?;
        let changes = prepared.what_if();
        ok(json!({"status": "Succeeded", "properties": {"changes": changes}}))
    }

    /// `POST /providers/Microsoft.Resources/calculateTemplateHash`.
    pub(super) fn calculate_template_hash(&self) -> Result<Response, ArmError> {
        if !self.body.is_object() {
            return Err(bad_content("The request content must be a template."));
        }
        ok(json!({"minifiedTemplate": self.body.to_string(), "templateHash": template_hash(self.body)}))
    }
}

fn ok(body: Value) -> Result<Response, ArmError> {
    Ok(json_response(StatusCode::OK, &body))
}

/// Forgets the deployments in a deleted resource group.
pub(super) fn forget_group(engine: &crate::Engine, group_id: &str) {
    engine.state::<DeploymentState>().lock().remove_group(group_id);
}
