//! The template deployment engine: parameters, variables, resource expansion
//! (copy loops, child resources, conditions), dependency ordering, execution and
//! outputs.
//!
//! Every resource is applied by sending its `PUT` through the same Resource
//! Manager routing a client's request takes, so a template can create anything
//! the emulator implements, and fails the way a direct request would for
//! anything it does not.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use chrono::{DateTime, Utc};
use http::{Method, StatusCode};
use serde_json::{json, Map, Value};

use super::functions::{id_from_parts, invalid, member, to_text, values_equal, Locals};
use crate::{ArmError, Config};

/// How the engine reaches the rest of Resource Manager.
pub trait Dispatch {
    /// Serves one internal request; an error status comes back as `Err`.
    fn call(&self, method: Method, path: &str, api_version: &str, body: &Value) -> Result<(StatusCode, Value), ArmError>;
    fn config(&self) -> &Config;
    /// The value of the `environment()` function.
    fn environment(&self) -> Value;
    /// An `api-version` the emulator serves `resource_type` on, for requests the
    /// template does not give one for (deleting in complete mode).
    fn api_version_for(&self, resource_type: &str) -> Option<String>;
    /// A Key Vault secret's value, for a parameter that references it.
    fn secret(&self, vault: &str, name: &str, version: Option<&str>) -> Option<String>;
}

/// The settings of one deployment.
pub struct Request<'a> {
    pub dispatch: &'a dyn Dispatch,
    pub subscription_id: String,
    /// `None` for a subscription-scoped deployment.
    pub resource_group: Option<String>,
    pub name: String,
    pub id: String,
    pub location: Option<String>,
    pub template: Value,
    pub parameters: Map<String, Value>,
    pub complete: bool,
}

/// What running a deployment produced.
pub struct Outcome {
    pub outputs: Option<Value>,
    pub output_resources: Vec<String>,
    pub operations: Vec<Value>,
    pub error: Option<Value>,
}

/// One known resource: what `reference()` and `dependsOn` can name.
#[derive(Clone, Debug)]
struct Target {
    symbol: Option<String>,
    copy_name: Option<String>,
    name: String,
    resource_type: String,
    id: String,
    api_version: String,
}

/// The evaluation context of one deployment.
pub struct Ctx<'a> {
    pub dispatch: &'a dyn Dispatch,
    pub subscription_id: String,
    pub resource_group: Option<String>,
    pub started: DateTime<Utc>,
    deployment_name: String,
    deployment_location: Option<String>,
    template: Value,
    parameters: RefCell<Map<String, Value>>,
    variables: Map<String, Value>,
    variable_cache: RefCell<HashMap<String, Value>>,
    variables_active: RefCell<HashSet<String>>,
    functions: HashMap<String, Value>,
    targets: RefCell<Vec<Target>>,
    complete: bool,
}

/// One resource instance after loops are expanded.
struct Instance {
    target: Target,
    existing: bool,
    condition: bool,
    definition: Value,
    locals: Locals,
    depends_on: Vec<String>,
    parent: Option<usize>,
}

/// A deployment that passed validation and is ready to run.
pub struct Prepared<'a> {
    pub ctx: Ctx<'a>,
    instances: Vec<Instance>,
    order: Vec<usize>,
    /// The instance indexes each instance waits for.
    edges: Vec<Vec<usize>>,
    id: String,
}

/// Members of a resource definition that describe it rather than being sent.
const META: &[&str] = &[
    "type", "name", "apiVersion", "dependsOn", "condition", "copy", "comments", "resources", "scope",
    "existing", "subscriptionId", "resourceGroup", "metadata", "asserts", "import",
];

fn lower_keys(map: &Map<String, Value>) -> HashMap<String, (&String, &Value)> {
    map.iter().map(|(k, v)| (k.to_ascii_lowercase(), (k, v))).collect()
}

/// The type names Resource Manager reports for parameters and outputs.
pub fn canonical_type(declared: &str) -> String {
    match declared.to_ascii_lowercase().as_str() {
        "string" => "String".into(),
        "securestring" => "SecureString".into(),
        "int" => "Int".into(),
        "bool" => "Bool".into(),
        "object" => "Object".into(),
        "secureobject" => "SecureObject".into(),
        "array" => "Array".into(),
        _ => declared.to_string(),
    }
}

fn is_secure(declared: &str) -> bool {
    matches!(declared.to_ascii_lowercase().as_str(), "securestring" | "secureobject")
}

/// A time as Resource Manager writes it, with seven fractional digits.
pub fn timestamp(time: &DateTime<Utc>) -> String {
    use chrono::Timelike;
    format!("{}.{:07}Z", time.format("%Y-%m-%dT%H:%M:%S"), time.nanosecond() % 1_000_000_000 / 100)
}

fn now_text() -> String {
    timestamp(&Utc::now())
}

fn duration_text(start: Instant) -> String {
    format!("PT{:.7}S", start.elapsed().as_secs_f64())
}

fn operation_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_ascii_uppercase()
}

impl<'a> Ctx<'a> {
    pub fn parameter(&self, name: &str) -> Result<Value, ArmError> {
        let parameters = self.parameters.borrow();
        member(&parameters, name)
            .cloned()
            .ok_or_else(|| invalid(format!("The template parameter '{name}' is not found. Please see https://aka.ms/arm-syntax-parameters for usage details.")))
    }

    pub fn variable(&self, name: &str) -> Result<Value, ArmError> {
        let key = name.to_ascii_lowercase();
        if let Some(value) = self.variable_cache.borrow().get(&key) {
            return Ok(value.clone());
        }
        if !self.variables_active.borrow_mut().insert(key.clone()) {
            return Err(invalid(format!("The template variable '{name}' refers to itself.")));
        }
        let result = self.compute_variable(name);
        self.variables_active.borrow_mut().remove(&key);
        let value = result?;
        self.variable_cache.borrow_mut().insert(key, value.clone());
        Ok(value)
    }

    fn compute_variable(&self, name: &str) -> Result<Value, ArmError> {
        if let Some(raw) = self.variables.iter().find(|(k, _)| *k != "copy" && k.eq_ignore_ascii_case(name)).map(|(_, v)| v) {
            return self.eval_value(raw, &Locals::default());
        }
        // `"copy": [{"name", "count", "input"}]` defines array variables.
        let loops = self.variables.get("copy").and_then(Value::as_array).cloned().unwrap_or_default();
        for entry in &loops {
            if entry.get("name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(name)) {
                let count = self.eval_value(entry.get("count").unwrap_or(&json!(0)), &Locals::default())?;
                let count = count.as_i64().ok_or_else(|| invalid("A copy count must be an integer."))?;
                let input = entry.get("input").cloned().unwrap_or(Value::Null);
                let loop_name = entry["name"].as_str().unwrap_or_default();
                return (0..count.max(0))
                    .map(|i| self.eval_value(&input, &Locals::default().with_copy(loop_name, i)))
                    .collect::<Result<_, _>>()
                    .map(Value::Array);
            }
        }
        Err(invalid(format!("The template variable '{name}' is not found. Please see https://aka.ms/arm-syntax-variables for usage details.")))
    }

    pub fn user_function(&self, name: &str) -> Option<&Value> {
        self.functions.get(&name.to_ascii_lowercase())
    }

    pub fn subscription_value(&self) -> Value {
        json!({
            "id": format!("/subscriptions/{}", self.subscription_id),
            "subscriptionId": self.subscription_id,
            "tenantId": self.dispatch.config().tenant_id,
            "displayName": "Azure Emulator",
        })
    }

    pub fn resource_group_value(&self) -> Result<Value, ArmError> {
        let group = self.resource_group.as_ref().ok_or_else(|| {
            invalid("The template function 'resourceGroup' is not expected at this location: the deployment is not scoped to a resource group.")
        })?;
        let path = format!("/subscriptions/{}/resourceGroups/{group}", self.subscription_id);
        Ok(self.dispatch.call(Method::GET, &path, "2021-04-01", &Value::Null)?.1)
    }

    pub fn deployment_value(&self) -> Value {
        let mut template = json!({
            "contentVersion": self.template.get("contentVersion").cloned().unwrap_or(json!("1.0.0.0")),
        });
        if let Some(metadata) = self.template.get("metadata") {
            template["metadata"] = metadata.clone();
        }
        let mut out = json!({
            "name": self.deployment_name,
            "properties": {
                "template": template,
                "templateHash": super::template_hash(&self.template),
                "mode": if self.complete { "Complete" } else { "Incremental" },
                "provisioningState": "Accepted",
            },
        });
        if let Some(location) = &self.deployment_location {
            out["location"] = json!(location);
        }
        out
    }

    pub fn list_action(&self, id: &str, function: &str, api_version: &str, body: Option<&Value>) -> Result<Value, ArmError> {
        let path = format!("{}/{function}", id.trim_end_matches('/'));
        Ok(self.dispatch.call(Method::POST, &path, api_version, body.unwrap_or(&Value::Null))?.1)
    }

    fn find_target(&self, wanted: &str) -> Option<Target> {
        let targets = self.targets.borrow();
        let found = if wanted.starts_with('/') {
            let wanted = normalize_id(wanted);
            targets.iter().find(|t| normalize_id(&t.id) == wanted)
        } else {
            targets
                .iter()
                .find(|t| t.symbol.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(wanted)))
                .or_else(|| targets.iter().find(|t| t.name.eq_ignore_ascii_case(wanted)))
        };
        found.cloned()
    }

    pub fn reference(&self, wanted: &str, api_version: Option<&str>, full: bool) -> Result<Value, ArmError> {
        let target = self.find_target(wanted);
        let id = match (&target, wanted.starts_with('/')) {
            (Some(t), _) => t.id.clone(),
            (None, true) => wanted.to_string(),
            (None, false) => {
                return Err(invalid(format!(
                    "The template reference '{wanted}' is not valid: could not find template resource or resource copy with this name."
                )))
            }
        };
        let api_version = api_version
            .map(str::to_string)
            .or_else(|| target.as_ref().map(|t| t.api_version.clone()))
            .ok_or_else(|| invalid(format!("The template function 'reference' needs an api-version for '{wanted}', which is not in the template.")))?;
        let (_, mut body) = self.dispatch.call(Method::GET, &id, &api_version, &Value::Null)?;
        if full {
            if let Some(object) = body.as_object_mut() {
                object.insert("apiVersion".into(), json!(api_version));
            }
            return Ok(body);
        }
        Ok(body.get("properties").cloned().unwrap_or_else(|| json!({})))
    }

    pub fn resource_info(&self, symbol: &str) -> Result<Value, ArmError> {
        let target = self
            .find_target(symbol)
            .ok_or_else(|| invalid(format!("The resource '{symbol}' is not defined in the template.")))?;
        Ok(json!({"id": target.id, "name": target.name, "type": target.resource_type, "apiVersion": target.api_version}))
    }
}

/// Resource groups are addressed both as `/resourceGroups/x` and as
/// `/providers/Microsoft.Resources/resourceGroups/x`.
fn normalize_id(id: &str) -> String {
    id.to_ascii_lowercase()
        .replace("/providers/microsoft.resources/resourcegroups/", "/resourcegroups/")
        .trim_end_matches('/')
        .to_string()
}

/// A parameter or output definition with any `$ref` to the template's
/// `definitions` (user-defined types) resolved; the definition's own members win.
fn resolve_definition(template: &Value, definition: &Value) -> Value {
    let mut current = definition.clone();
    for _ in 0..16 {
        let Some(reference) = current.get("$ref").and_then(Value::as_str) else {
            break;
        };
        let Some(target) = reference.strip_prefix('#').and_then(|p| template.pointer(p)).cloned() else {
            break;
        };
        let mut merged = target.as_object().cloned().unwrap_or_default();
        for (key, value) in current.as_object().into_iter().flatten() {
            if key != "$ref" {
                merged.insert(key.clone(), value.clone());
            }
        }
        current = Value::Object(merged);
    }
    current
}

/// Validates the template and parameters and expands the resources.
pub fn prepare<'a>(request: Request<'a>) -> Result<Prepared<'a>, ArmError> {
    let template = request.template;
    if !template.is_object() {
        return Err(invalid("Deployment template validation failed: the template must be a JSON object."));
    }
    let mut functions = HashMap::new();
    for namespace in template.get("functions").and_then(Value::as_array).into_iter().flatten() {
        let prefix = namespace.get("namespace").and_then(Value::as_str).unwrap_or_default();
        for (name, definition) in namespace.get("members").and_then(Value::as_object).into_iter().flatten() {
            functions.insert(format!("{prefix}.{name}").to_ascii_lowercase(), definition.clone());
        }
    }
    let ctx = Ctx {
        dispatch: request.dispatch,
        subscription_id: request.subscription_id,
        resource_group: request.resource_group,
        started: Utc::now(),
        deployment_name: request.name,
        deployment_location: request.location,
        variables: template.get("variables").and_then(Value::as_object).cloned().unwrap_or_default(),
        parameters: RefCell::new(Map::new()),
        variable_cache: RefCell::new(HashMap::new()),
        variables_active: RefCell::new(HashSet::new()),
        functions,
        targets: RefCell::new(Vec::new()),
        complete: request.complete,
        template,
    };
    bind_parameters(&ctx, &request.parameters)?;

    let mut instances = Vec::new();
    match ctx.template.get("resources") {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) => {
            for definition in items {
                expand(&ctx, definition, None, None, &Locals::default(), &mut instances)?;
            }
        }
        // languageVersion 2.0: resources keyed by symbolic name.
        Some(Value::Object(items)) => {
            for (symbol, definition) in items {
                expand(&ctx, definition, Some(symbol), None, &Locals::default(), &mut instances)?;
            }
        }
        Some(_) => return Err(invalid("Deployment template validation failed: 'resources' must be an array or an object.")),
    }
    *ctx.targets.borrow_mut() = instances.iter().map(|i| i.target.clone()).collect();

    let edges = resolve_dependencies(&instances)?;
    let order = topological_order(&instances, &edges)?;
    Ok(Prepared {
        ctx,
        instances,
        order,
        edges,
        id: request.id,
    })
}

fn bind_parameters(ctx: &Ctx<'_>, provided: &Map<String, Value>) -> Result<(), ArmError> {
    let definitions = ctx.template.get("parameters").and_then(Value::as_object).cloned().unwrap_or_default();
    let by_lower = lower_keys(&definitions);
    let unknown: Vec<&str> = provided
        .keys()
        .filter(|k| !by_lower.contains_key(&k.to_ascii_lowercase()))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err(invalid(format!(
            "Deployment template validation failed: 'The template parameters '{}' in the parameters file are not valid; they are not present in the original template and can therefore not be provided at deployment time. The only supported parameters for this template are '{}'.'",
            unknown.join(", "),
            definitions.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }
    for (name, definition) in &definitions {
        let definition = &resolve_definition(&ctx.template, definition);
        let given = provided.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v);
        let value = match given {
            Some(entry) if entry.get("value").is_some() => entry["value"].clone(),
            Some(entry) if entry.get("reference").is_some() => key_vault_reference(ctx, name, &entry["reference"])?,
            Some(_) => {
                return Err(ArmError::bad_request(
                    "InvalidRequestContent",
                    format!("The value of deployment parameter '{name}' is null. Please specify the value or use the parameter reference."),
                ))
            }
            None => match definition.get("defaultValue") {
                Some(default) => ctx.eval_value(default, &Locals::default())?,
                None if definition.get("nullable").and_then(Value::as_bool) == Some(true) => Value::Null,
                None => {
                    return Err(invalid(format!(
                        "Deployment template validation failed: 'The value for the template parameter '{name}' is not provided. Please see https://aka.ms/arm-create-parameter-file for usage details.'."
                    )))
                }
            },
        };
        check_parameter(name, definition, &value)?;
        ctx.parameters.borrow_mut().insert(name.clone(), value);
    }
    Ok(())
}

/// A parameter's value from a Key Vault secret:
/// `{"keyVault": {"id": ...}, "secretName": ..., "secretVersion": ...}`. As in
/// Azure, the vault must exist and allow template deployment
/// (`enabledForTemplateDeployment`).
fn key_vault_reference(ctx: &Ctx<'_>, parameter: &str, reference: &Value) -> Result<Value, ArmError> {
    // Unverified against Azure: the messages; the code is the one Azure documents.
    let failed = |message: String| ArmError::bad_request("KeyVaultParameterReferenceSecretRetrieveFailed", message);
    let vault_id = reference
        .pointer("/keyVault/id")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("The Key Vault reference of parameter '{parameter}' has no 'keyVault.id'.")))?;
    let secret = reference
        .get("secretName")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("The Key Vault reference of parameter '{parameter}' has no 'secretName'.")))?;
    let version = reference.get("secretVersion").and_then(Value::as_str).filter(|v| !v.is_empty());
    let segments: Vec<&str> = vault_id.trim_end_matches('/').split('/').collect();
    let is_vault = segments.len() == 9 && segments[6].eq_ignore_ascii_case("Microsoft.KeyVault") && segments[7].eq_ignore_ascii_case("vaults");
    if !is_vault {
        return Err(invalid(format!("'{vault_id}' in parameter '{parameter}' is not the id of a Key Vault.")));
    }
    let vault_name = segments[8];
    let api_version = ctx.dispatch.api_version_for("Microsoft.KeyVault/vaults").unwrap_or_else(|| "2023-07-01".into());
    let vault = ctx
        .dispatch
        .call(Method::GET, vault_id, &api_version, &Value::Null)
        .map_err(|_| failed(format!("The specified KeyVault '{vault_id}' could not be found. Please see https://aka.ms/arm-keyvault for usage details.")))?
        .1;
    if vault.pointer("/properties/enabledForTemplateDeployment") != Some(&Value::Bool(true)) {
        return Err(failed(format!(
            "The key vault '{vault_id}' is not enabled for template deployment ('enabledForTemplateDeployment'); the value of parameter '{parameter}' cannot be retrieved. Please see https://aka.ms/arm-keyvault for usage details."
        )));
    }
    let value = ctx.dispatch.secret(vault_name, secret, version).ok_or_else(|| {
        failed(format!(
            "The secret '{secret}'{} could not be retrieved from Key Vault '{vault_id}' for parameter '{parameter}'. Please see https://aka.ms/arm-keyvault for usage details.",
            version.map(|v| format!(" (version '{v}')")).unwrap_or_default()
        ))
    })?;
    Ok(Value::String(value))
}

fn check_parameter(name: &str, definition: &Value, value: &Value) -> Result<(), ArmError> {
    if value.is_null() {
        return Ok(());
    }
    let declared = definition.get("type").and_then(Value::as_str).unwrap_or("");
    let ok = match declared.to_ascii_lowercase().as_str() {
        "string" | "securestring" => value.is_string(),
        "int" => value.is_i64() || value.is_u64(),
        "bool" => value.is_boolean(),
        "object" | "secureobject" => value.is_object(),
        "array" => value.is_array(),
        _ => true,
    };
    if !ok {
        return Err(invalid(format!(
            "Deployment template validation failed: 'Template parameter JToken type is not valid. Expected '{}'. Actual '{}'. The parameter is '{name}'.'",
            canonical_type(declared),
            match value {
                Value::String(_) => "String",
                Value::Bool(_) => "Boolean",
                Value::Number(_) => "Integer",
                Value::Array(_) => "Array",
                Value::Object(_) => "Object",
                Value::Null => "Null",
            }
        )));
    }
    if let Some(allowed) = definition.get("allowedValues").and_then(Value::as_array) {
        let items: Vec<&Value> = match value {
            Value::Array(items) if !declared.eq_ignore_ascii_case("object") => items.iter().collect(),
            other => vec![other],
        };
        if let Some(bad) = items.iter().find(|item| !allowed.iter().any(|a| values_equal(a, item))) {
            return Err(invalid(format!(
                "Deployment template validation failed: 'The provided value '{}' for the template parameter '{name}' is not valid. The parameter value is not part of the allowed value(s): '{}'.'.",
                to_text(bad),
                allowed.iter().map(to_text).collect::<Vec<_>>().join(",")
            )));
        }
    }
    let length = match value {
        Value::String(s) => Some(s.chars().count() as i64),
        Value::Array(a) => Some(a.len() as i64),
        _ => None,
    };
    if let Some(length) = length {
        if let Some(min) = definition.get("minLength").and_then(Value::as_i64).filter(|m| length < *m) {
            return Err(invalid(format!(
                "Deployment template validation failed: 'The provided value for the template parameter '{name}' is not valid. Length of the value should be greater than or equal to '{min}'.'."
            )));
        }
        if let Some(max) = definition.get("maxLength").and_then(Value::as_i64).filter(|m| length > *m) {
            return Err(invalid(format!(
                "Deployment template validation failed: 'The provided value for the template parameter '{name}' is not valid. Length of the value should be less than or equal to '{max}'.'."
            )));
        }
    }
    if let Some(number) = value.as_i64() {
        if let Some(min) = definition.get("minValue").and_then(Value::as_i64).filter(|m| number < *m) {
            return Err(invalid(format!(
                "Deployment template validation failed: 'The provided value for the template parameter '{name}' is not valid. The value should be greater than or equal to '{min}'.'."
            )));
        }
        if let Some(max) = definition.get("maxValue").and_then(Value::as_i64).filter(|m| number > *m) {
            return Err(invalid(format!(
                "Deployment template validation failed: 'The provided value for the template parameter '{name}' is not valid. The value should be less than or equal to '{max}'.'."
            )));
        }
    }
    Ok(())
}

/// The parent of a child resource declared in another resource's `resources`.
struct Parent<'p> {
    index: usize,
    resource_type: &'p str,
    name: &'p str,
    condition: bool,
}

fn string_member(ctx: &Ctx<'_>, definition: &Value, key: &str, locals: &Locals) -> Result<Option<String>, ArmError> {
    match definition.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(raw) => match ctx.eval_value(raw, locals)? {
            Value::String(s) => Ok(Some(s)),
            Value::Null => Ok(None),
            other => Err(invalid(format!("The template resource property '{key}' must be a string, not '{}'.", to_text(&other)))),
        },
    }
}

fn expand(
    ctx: &Ctx<'_>,
    definition: &Value,
    symbol: Option<&str>,
    parent: Option<&Parent<'_>>,
    locals: &Locals,
    out: &mut Vec<Instance>,
) -> Result<(), ArmError> {
    if !definition.is_object() {
        return Err(invalid("Deployment template validation failed: a template resource must be a JSON object."));
    }
    match definition.get("copy") {
        Some(copy) if copy.is_object() => {
            let loop_name = copy.get("name").and_then(Value::as_str).ok_or_else(|| invalid("A resource copy loop needs a name."))?;
            let count = ctx.eval_value(copy.get("count").unwrap_or(&Value::Null), locals)?;
            let count = count
                .as_i64()
                .filter(|c| (0..=800).contains(c))
                .ok_or_else(|| invalid(format!("The copy count of loop '{loop_name}' must be an integer from 0 to 800.")))?;
            for index in 0..count {
                expand_one(ctx, definition, symbol, Some(loop_name), parent, &locals.with_copy(loop_name, index), out)?;
            }
            Ok(())
        }
        _ => expand_one(ctx, definition, symbol, None, parent, locals, out),
    }
}

fn expand_one(
    ctx: &Ctx<'_>,
    definition: &Value,
    symbol: Option<&str>,
    copy_name: Option<&str>,
    parent: Option<&Parent<'_>>,
    locals: &Locals,
    out: &mut Vec<Instance>,
) -> Result<(), ArmError> {
    let declared_type = string_member(ctx, definition, "type", locals)?
        .ok_or_else(|| invalid("Deployment template validation failed: a template resource has no 'type'."))?;
    let declared_name = string_member(ctx, definition, "name", locals)?
        .ok_or_else(|| invalid(format!("Deployment template validation failed: a resource of type '{declared_type}' has no 'name'.")))?;
    // A child declared inside its parent may give a relative type and name.
    let (resource_type, name) = match parent {
        Some(p) if !declared_type.contains('/') => (
            format!("{}/{declared_type}", p.resource_type),
            format!("{}/{declared_name}", p.name),
        ),
        _ => (declared_type.clone(), declared_name.clone()),
    };
    let existing = definition.get("existing").and_then(Value::as_bool).unwrap_or(false);
    let api_version = match definition.get("apiVersion").and_then(Value::as_str) {
        Some(v) => v.to_string(),
        None if existing => String::new(),
        None => {
            return Err(invalid(format!(
                "Deployment template validation failed: 'The template resource '{name}' of type '{resource_type}' does not have an apiVersion.'"
            )))
        }
    };
    let condition = match definition.get("condition") {
        None => true,
        Some(raw) => match ctx.eval_value(raw, locals)? {
            Value::Bool(b) => b,
            Value::String(s) if s.eq_ignore_ascii_case("true") => true,
            Value::String(s) if s.eq_ignore_ascii_case("false") => false,
            other => return Err(invalid(format!("The condition of template resource '{name}' must be a boolean, not '{}'.", to_text(&other)))),
        },
    } && parent.is_none_or(|p| p.condition);

    let id = locate(ctx, definition, &resource_type, &name, locals)?;
    let depends_on = match definition.get("dependsOn") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| ctx.eval_value(item, locals).map(|v| to_text(&v)))
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(invalid(format!("The 'dependsOn' of template resource '{name}' must be an array."))),
    };
    let index = out.len();
    out.push(Instance {
        target: Target {
            symbol: symbol.map(str::to_string),
            copy_name: copy_name.map(str::to_string),
            name: name.clone(),
            resource_type: resource_type.clone(),
            id,
            api_version,
        },
        existing,
        condition,
        definition: definition.clone(),
        locals: locals.clone(),
        depends_on,
        parent: parent.map(|p| p.index),
    });
    if let Some(children) = definition.get("resources").and_then(Value::as_array) {
        let me = Parent {
            index,
            resource_type: &resource_type,
            name: &name,
            condition,
        };
        for child in children {
            expand(ctx, child, None, Some(&me), locals, out)?;
        }
    }
    Ok(())
}

/// The id of a template resource, which is also where it is sent.
fn locate(ctx: &Ctx<'_>, definition: &Value, resource_type: &str, name: &str, locals: &Locals) -> Result<String, ArmError> {
    let subscription = string_member(ctx, definition, "subscriptionId", locals)?.unwrap_or_else(|| ctx.subscription_id.clone());
    let group = string_member(ctx, definition, "resourceGroup", locals)?;
    if resource_type.eq_ignore_ascii_case("Microsoft.Resources/resourceGroups") {
        return Ok(format!("/subscriptions/{subscription}/resourceGroups/{name}"));
    }
    let names: Vec<&str> = name.split('/').collect();
    let base = if resource_type.eq_ignore_ascii_case("Microsoft.Resources/deployments") {
        match group.or_else(|| ctx.resource_group.clone()) {
            Some(group) => format!("/subscriptions/{subscription}/resourceGroups/{group}"),
            None => format!("/subscriptions/{subscription}"),
        }
    } else {
        let scope_base = match &ctx.resource_group {
            Some(group) => format!("/subscriptions/{subscription}/resourceGroups/{group}"),
            None => format!("/subscriptions/{subscription}"),
        };
        match string_member(ctx, definition, "scope", locals)? {
            Some(scope) if scope.starts_with('/') => scope.trim_end_matches('/').to_string(),
            // A relative scope names a resource in the deployment's scope.
            Some(scope) => format!("{scope_base}/providers/{}", scope.trim_matches('/')),
            None => scope_base,
        }
    };
    id_from_parts(&base, resource_type, &names).ok_or_else(|| {
        invalid(format!(
            "Deployment template validation failed: 'The template resource '{name}' for type '{resource_type}' has incorrect segment lengths. A nested resource type must have identical number of segments as its resource name. A root resource type must have segment length one greater than its resource name.'"
        ))
    })
}

fn resolve_dependencies(instances: &[Instance]) -> Result<Vec<Vec<usize>>, ArmError> {
    let mut edges = Vec::with_capacity(instances.len());
    for (index, instance) in instances.iter().enumerate() {
        let mut deps: Vec<usize> = instance.parent.into_iter().collect();
        for wanted in &instance.depends_on {
            let lower = wanted.to_ascii_lowercase();
            let normalized = normalize_id(wanted);
            let matches: Vec<usize> = instances
                .iter()
                .enumerate()
                .filter(|(other, _)| *other != index)
                .filter(|(_, candidate)| {
                    let t = &candidate.target;
                    let id = normalize_id(&t.id);
                    t.symbol.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(wanted))
                        || t.copy_name.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(wanted))
                        || id == normalized
                        || t.name.eq_ignore_ascii_case(wanted)
                        || id.ends_with(&format!("/{lower}"))
                })
                .map(|(other, _)| other)
                .collect();
            if matches.is_empty() {
                return Err(invalid(format!(
                    "Deployment template validation failed: 'The resource '{wanted}' is not defined in the template. Please see https://aka.ms/arm-syntax for usage details.'."
                )));
            }
            deps.extend(matches);
        }
        deps.sort_unstable();
        deps.dedup();
        edges.push(deps);
    }
    Ok(edges)
}

/// Declaration order, except that a resource comes after what it depends on.
fn topological_order(instances: &[Instance], edges: &[Vec<usize>]) -> Result<Vec<usize>, ArmError> {
    let mut placed = vec![false; instances.len()];
    let mut order = Vec::with_capacity(instances.len());
    while order.len() < instances.len() {
        let next = (0..instances.len()).find(|i| !placed[*i] && edges[*i].iter().all(|d| placed[*d]));
        match next {
            Some(i) => {
                placed[i] = true;
                order.push(i);
            }
            None => {
                let stuck: Vec<String> = (0..instances.len())
                    .filter(|i| !placed[*i])
                    .map(|i| instances[i].target.name.clone())
                    .collect();
                return Err(invalid(format!(
                    "Deployment template validation failed: 'Circular dependency detected on resource: '{}'. Please see https://aka.ms/arm-syntax-resources for usage details.'.",
                    stuck.join("', '")
                )));
            }
        }
    }
    Ok(order)
}

impl Prepared<'_> {
    /// The ids of the resources the deployment will create or update.
    pub fn validated_resources(&self) -> Vec<Value> {
        self.instances
            .iter()
            .filter(|i| i.condition && !i.existing)
            .map(|i| json!({"id": i.target.id}))
            .collect()
    }

    /// The deployment's `parameters` as Resource Manager reports them.
    pub fn parameters_view(&self) -> Value {
        let definitions = self.ctx.template.get("parameters").and_then(Value::as_object).cloned().unwrap_or_default();
        let parameters = self.ctx.parameters.borrow();
        let mut out = Map::new();
        for (name, definition) in &definitions {
            let definition = resolve_definition(&self.ctx.template, definition);
            let declared = definition.get("type").and_then(Value::as_str).unwrap_or("Object");
            let mut entry = json!({"type": canonical_type(declared)});
            if !is_secure(declared) {
                entry["value"] = parameters.get(name).cloned().unwrap_or(Value::Null);
            }
            out.insert(name.clone(), entry);
        }
        Value::Object(out)
    }

    /// The `dependencies` Resource Manager reports.
    pub fn dependencies_view(&self) -> Value {
        let entry = |t: &Target| json!({"id": t.id, "resourceType": t.resource_type, "resourceName": t.name});
        Value::Array(
            self.instances
                .iter()
                .zip(&self.edges)
                .filter(|(_, deps)| !deps.is_empty())
                .map(|(instance, deps)| {
                    let mut out = entry(&instance.target);
                    out["dependsOn"] = Value::Array(deps.iter().map(|d| entry(&self.instances[*d].target)).collect());
                    out
                })
                .collect(),
        )
    }

    /// The `providers` Resource Manager reports: each namespace and type used.
    pub fn providers_view(&self) -> Value {
        let mut namespaces: Vec<(String, Vec<String>)> = Vec::new();
        for instance in &self.instances {
            let Some((namespace, kind)) = instance.target.resource_type.split_once('/') else {
                continue;
            };
            let position = match namespaces.iter().position(|(n, _)| n.eq_ignore_ascii_case(namespace)) {
                Some(p) => p,
                None => {
                    namespaces.push((namespace.to_string(), Vec::new()));
                    namespaces.len() - 1
                }
            };
            if !namespaces[position].1.iter().any(|k| k.eq_ignore_ascii_case(kind)) {
                namespaces[position].1.push(kind.to_string());
            }
        }
        Value::Array(
            namespaces
                .into_iter()
                .map(|(namespace, kinds)| {
                    json!({
                        "namespace": namespace,
                        "resourceTypes": kinds.iter().map(|k| json!({"resourceType": k, "locations": []})).collect::<Vec<_>>(),
                    })
                })
                .collect(),
        )
    }

    /// The request body a resource is sent with.
    fn body(&self, instance: &Instance) -> Result<Value, ArmError> {
        let mut body = instance.definition.as_object().cloned().unwrap_or_default();
        body.retain(|k, _| !META.iter().any(|m| m.eq_ignore_ascii_case(k)));
        let is_deployment = instance.target.resource_type.eq_ignore_ascii_case("Microsoft.Resources/deployments");
        // A nested deployment's template belongs to that deployment: it is not
        // evaluated here.
        let nested_template = match (is_deployment, body.get_mut("properties").and_then(Value::as_object_mut)) {
            (true, Some(properties)) => properties.remove("template"),
            _ => None,
        };
        let mut body = self.ctx.eval_value(&Value::Object(body), &instance.locals)?;
        if let Some(template) = nested_template {
            let inner = body
                .pointer("/properties/expressionEvaluationOptions/scope")
                .and_then(Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case("inner"));
            let properties = body
                .get_mut("properties")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| invalid("A nested deployment needs 'properties'."))?;
            if inner {
                properties.insert("template".into(), template);
            } else {
                // Outer scope: the nested template sees this template's parameters,
                // variables and functions; pass them down as its own.
                let (template, parameters) = self.outer_scope(template);
                properties.insert("template".into(), template);
                properties.insert("parameters".into(), parameters);
                properties.insert("expressionEvaluationOptions".into(), json!({"scope": "inner"}));
            }
        }
        Ok(body)
    }

    fn outer_scope(&self, mut template: Value) -> (Value, Value) {
        let definitions = self.ctx.template.get("parameters").and_then(Value::as_object).cloned().unwrap_or_default();
        let parameters = self.ctx.parameters.borrow();
        let mut declared = Map::new();
        let mut values = Map::new();
        for (name, definition) in definitions {
            let kind = definition.get("type").cloned().unwrap_or(json!("object"));
            declared.insert(name.clone(), json!({"type": kind}));
            values.insert(name.clone(), json!({"value": parameters.get(&name).cloned().unwrap_or(Value::Null)}));
        }
        if let Some(object) = template.as_object_mut() {
            object.insert("parameters".into(), Value::Object(declared));
            object.insert("variables".into(), Value::Object(self.ctx.variables.clone()));
            if let Some(functions) = self.ctx.template.get("functions") {
                object.insert("functions".into(), functions.clone());
            }
        }
        (template, Value::Object(values))
    }

    /// Sends one resource. A failure carries the nested deployment's own error
    /// when the resource is a deployment.
    fn apply(&self, instance: &Instance) -> Result<StatusCode, (ArmError, Option<Value>)> {
        let target = &instance.target;
        let body = self.body(instance).map_err(|e| (e, None))?;
        let (status, reply) = self
            .ctx
            .dispatch
            .call(Method::PUT, &target.id, &target.api_version, &body)
            .map_err(|e| (e, None))?;
        let is_deployment = target.resource_type.eq_ignore_ascii_case("Microsoft.Resources/deployments");
        if is_deployment && reply.pointer("/properties/provisioningState").and_then(Value::as_str) == Some("Failed") {
            let error = reply.pointer("/properties/error").cloned().unwrap_or(json!({}));
            let failure = ArmError::bad_request(
                error.get("code").and_then(Value::as_str).unwrap_or("DeploymentFailed"),
                error.get("message").and_then(Value::as_str).unwrap_or("The nested deployment failed."),
            );
            return Err((failure, Some(error)));
        }
        Ok(status)
    }

    /// Runs the deployment.
    pub fn execute(&self) -> Outcome {
        let mut operations = Vec::new();
        let mut failures: Vec<Value> = Vec::new();
        let mut failed = vec![false; self.instances.len()];
        let mut output_resources = Vec::new();
        for &index in &self.order {
            let instance = &self.instances[index];
            if !instance.condition || instance.existing {
                continue;
            }
            if self.edges[index].iter().any(|d| failed[*d]) {
                failed[index] = true;
                continue;
            }
            let started = Instant::now();
            let result = self.apply(instance);
            let target = &instance.target;
            let mut operation = json!({
                "id": format!("{}/operations/{}", self.id, operation_id()),
                "properties": {
                    "provisioningOperation": "Create",
                    "timestamp": now_text(),
                    "duration": duration_text(started),
                    "trackingId": uuid::Uuid::new_v4().to_string(),
                    "targetResource": {
                        "id": target.id,
                        "resourceType": target.resource_type,
                        "resourceName": target.name,
                        "apiVersion": target.api_version,
                    },
                },
            });
            operation["operationId"] = json!(operation["id"].as_str().unwrap_or("").rsplit('/').next().unwrap_or(""));
            match result {
                Ok(status) => {
                    operation["properties"]["provisioningState"] = json!("Succeeded");
                    operation["properties"]["statusCode"] = json!(status_name(status));
                    output_resources.push(json!({"id": target.id}));
                }
                Err((error, nested)) => {
                    failed[index] = true;
                    let detail = match nested {
                        Some(nested) => nested,
                        None => json!({"code": error.code, "message": error.message}),
                    };
                    operation["properties"]["provisioningState"] = json!("Failed");
                    operation["properties"]["statusCode"] = json!(status_name(error.status));
                    operation["properties"]["statusMessage"] = json!({"status": "Failed", "error": detail});
                    failures.push(detail);
                }
            }
            operations.push(operation);
        }
        if !failures.is_empty() {
            return Outcome {
                outputs: None,
                output_resources: output_resources.iter().filter_map(|r| r["id"].as_str().map(String::from)).collect(),
                operations,
                error: Some(json!({
                    "code": "DeploymentFailed",
                    "message": "At least one resource deployment operation failed. Please list deployment operations for details. Please see https://aka.ms/arm-deployment-operations for usage details.",
                    "details": failures,
                })),
            };
        }
        if self.ctx.complete {
            if let Err(error) = self.delete_unlisted() {
                return Outcome {
                    outputs: None,
                    output_resources: Vec::new(),
                    operations,
                    error: Some(json!({"code": error.code, "message": error.message})),
                };
            }
        }
        let (outputs, error) = match self.outputs() {
            Ok(outputs) => (Some(outputs), None),
            Err(error) => (
                None,
                Some(json!({
                    "code": "DeploymentOutputEvaluationFailed",
                    "message": "Unable to evaluate template outputs. Please see error details and deployment operations. Please see https://aka.ms/arm-common-errors for usage details.",
                    "details": [{"code": error.code, "message": error.message}],
                })),
            ),
        };
        Outcome {
            outputs,
            output_resources: output_resources.iter().filter_map(|r| r["id"].as_str().map(String::from)).collect(),
            operations,
            error,
        }
    }

    /// The ids complete mode keeps: everything the template names, deployed or not.
    fn kept(&self) -> Vec<String> {
        self.instances.iter().map(|i| normalize_id(&i.target.id)).collect()
    }

    /// The group's top-level resources the template does not name.
    fn unlisted(&self) -> Result<Vec<(String, String)>, ArmError> {
        let Some(group) = &self.ctx.resource_group else {
            return Ok(Vec::new());
        };
        let path = format!("/subscriptions/{}/resourceGroups/{group}/resources", self.ctx.subscription_id);
        let (_, listing) = self.ctx.dispatch.call(Method::GET, &path, "2021-04-01", &Value::Null)?;
        let kept = self.kept();
        Ok(listing
            .get("value")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|r| Some((r.get("id")?.as_str()?.to_string(), r.get("type")?.as_str()?.to_string())))
            .filter(|(id, _)| {
                let id = normalize_id(id);
                !kept.iter().any(|k| *k == id || k.starts_with(&format!("{id}/")))
            })
            .collect())
    }

    fn delete_unlisted(&self) -> Result<(), ArmError> {
        for (id, resource_type) in self.unlisted()? {
            let api_version = self.ctx.dispatch.api_version_for(&resource_type).ok_or_else(|| {
                ArmError::not_implemented(format_args!("deleting resources of type '{resource_type}' in complete mode"))
            })?;
            self.ctx.dispatch.call(Method::DELETE, &id, &api_version, &Value::Null)?;
        }
        Ok(())
    }

    fn outputs(&self) -> Result<Value, ArmError> {
        let definitions = self.ctx.template.get("outputs").and_then(Value::as_object).cloned().unwrap_or_default();
        let mut out = Map::new();
        let locals = Locals::default();
        for (name, definition) in &definitions {
            if let Some(condition) = definition.get("condition") {
                if self.ctx.eval_value(condition, &locals)? != Value::Bool(true) {
                    continue;
                }
            }
            let value = match definition.get("copy") {
                Some(copy) => {
                    let count = self.ctx.eval_value(copy.get("count").unwrap_or(&json!(0)), &locals)?;
                    let count = count.as_i64().ok_or_else(|| invalid("An output copy count must be an integer."))?;
                    let input = copy.get("input").cloned().unwrap_or(Value::Null);
                    (0..count.max(0))
                        .map(|i| self.ctx.eval_value(&input, &locals.with_copy(name, i)))
                        .collect::<Result<_, _>>()
                        .map(Value::Array)?
                }
                None => self.ctx.eval_value(definition.get("value").unwrap_or(&Value::Null), &locals)?,
            };
            let resolved = resolve_definition(&self.ctx.template, definition);
            let declared = resolved.get("type").and_then(Value::as_str).unwrap_or("Object");
            let mut entry = json!({"type": canonical_type(declared)});
            if !is_secure(declared) {
                entry["value"] = value;
            }
            out.insert(name.clone(), entry);
        }
        Ok(Value::Object(out))
    }

    /// What deploying would change, without changing anything.
    pub fn what_if(&self) -> Vec<Value> {
        let mut changes = Vec::new();
        for &index in &self.order {
            let instance = &self.instances[index];
            if !instance.condition || instance.existing {
                continue;
            }
            let target = &instance.target;
            let body = match self.body(instance) {
                Ok(body) => body,
                Err(error) => {
                    changes.push(json!({
                        "resourceId": target.id,
                        "changeType": "Unsupported",
                        "unsupportedReason": error.message,
                    }));
                    continue;
                }
            };
            if target.resource_type.eq_ignore_ascii_case("Microsoft.Resources/deployments") {
                // A nested deployment's changes are those of its own template.
                let path = format!("{}/whatIf", target.id);
                match self.ctx.dispatch.call(Method::POST, &path, &target.api_version, &body) {
                    Ok((_, reply)) => {
                        let nested = reply.pointer("/properties/changes").and_then(Value::as_array).cloned().unwrap_or_default();
                        changes.extend(nested);
                    }
                    Err(error) => changes.push(json!({
                        "resourceId": target.id,
                        "changeType": "Unsupported",
                        "unsupportedReason": error.message,
                    })),
                }
                continue;
            }
            let mut after = body.clone();
            if let Some(object) = after.as_object_mut() {
                object.insert("id".into(), json!(target.id));
                object.insert("name".into(), json!(target.name.rsplit('/').next().unwrap_or(&target.name)));
                object.insert("type".into(), json!(target.resource_type));
                object.insert("apiVersion".into(), json!(target.api_version));
            }
            match self.ctx.dispatch.call(Method::GET, &target.id, &target.api_version, &Value::Null) {
                Ok((_, mut before)) => {
                    if let Some(object) = before.as_object_mut() {
                        object.insert("apiVersion".into(), json!(target.api_version));
                    }
                    let mut delta = Vec::new();
                    diff("", &body, &before, &mut delta);
                    let change_type = if delta.is_empty() { "NoChange" } else { "Modify" };
                    let mut change = json!({"resourceId": target.id, "changeType": change_type, "before": before, "after": after});
                    if !delta.is_empty() {
                        change["delta"] = Value::Array(delta);
                    }
                    changes.push(change);
                }
                Err(error) if error.status == StatusCode::NOT_FOUND => {
                    changes.push(json!({"resourceId": target.id, "changeType": "Create", "after": after}));
                }
                Err(error) => changes.push(json!({
                    "resourceId": target.id,
                    "changeType": "Unsupported",
                    "unsupportedReason": error.message,
                })),
            }
        }
        if let Ok(unlisted) = self.unlisted() {
            let change_type = if self.ctx.complete { "Delete" } else { "Ignore" };
            for (id, _) in unlisted {
                changes.push(json!({"resourceId": id, "changeType": change_type}));
            }
        }
        changes
    }
}

/// The members of `requested` that differ from `current`, as what-if deltas.
fn diff(path: &str, requested: &Value, current: &Value, out: &mut Vec<Value>) {
    match (requested, current) {
        (Value::Object(want), Value::Object(have)) => {
            for (key, value) in want {
                let child = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
                match member(have, key) {
                    Some(existing) => diff(&child, value, existing, out),
                    None if value.is_null() => {}
                    None => out.push(json!({"path": child, "propertyChangeType": "Create", "after": value})),
                }
            }
        }
        (want, have) if values_equal(want, have) => {}
        (Value::String(want), Value::String(have)) if want.eq_ignore_ascii_case(have) && path.ends_with("location") => {}
        (want, have) => out.push(json!({"path": path, "propertyChangeType": "Modify", "before": have, "after": want})),
    }
}

fn status_name(status: StatusCode) -> String {
    match status {
        StatusCode::OK => "OK".into(),
        StatusCode::CREATED => "Created".into(),
        StatusCode::ACCEPTED => "Accepted".into(),
        StatusCode::NO_CONTENT => "NoContent".into(),
        other => other.canonical_reason().unwrap_or("Unknown").replace(' ', ""),
    }
}
