//! Object replication: the policies on Resource Manager, and the copying they
//! cause on the Blob data plane.
//!
//! A policy is created first on the destination account, named `default`; the
//! service assigns its policy id and rule ids. The same policy, with those ids,
//! is then created on the source account. Once a policy is installed on both
//! accounts, a blob written to a source container that a rule matches is copied
//! to the rule's destination container. The real service copies asynchronously;
//! the emulator copies before the write returns. Existing blobs are copied when
//! the policy is installed on the source only if a rule sets `minCreationTime`.
//!
//! Prerequisites are enforced: blob versioning on both accounts, change feed on
//! the source, and the rules' containers on the account being configured.
//!
//! Not modeled: the destination container becoming read-only, replication to an
//! account outside the emulator, and deletes (which the service never
//! replicates). Not checked against Azure: the refusal codes and wording.

use std::sync::Arc;
use std::time::SystemTime;

use emulator_core::arm::{ArmContext, ArmRegistry, ArmState, OperationRequest};
use emulator_core::http_util::iso_timestamp;
use emulator_core::ArmError;
use http::StatusCode;
use serde_json::{json, Value};

use crate::management::require_account;
use crate::StorageState;

pub(crate) struct Policy {
    /// Properties as the service returns them.
    properties: Value,
    destination: String,
    /// Whether this account is the policy's source.
    is_source: bool,
    rules: Vec<Rule>,
}

struct Rule {
    id: String,
    source_container: String,
    destination_container: String,
    prefixes: Vec<String>,
    min_creation: Option<SystemTime>,
}

impl Rule {
    fn matches(&self, container: &str, blob: &str) -> bool {
        self.source_container == container && (self.prefixes.is_empty() || self.prefixes.iter().any(|p| blob.starts_with(p.as_str())))
    }
}

pub(crate) fn register(registry: &mut ArmRegistry) {
    registry.register_operation("ObjectReplicationPolicies_List", Arc::new(list));
    registry.register_operation("ObjectReplicationPolicies_Get", Arc::new(get));
    registry.register_operation("ObjectReplicationPolicies_CreateOrUpdate", Arc::new(put));
    registry.register_operation("ObjectReplicationPolicies_Delete", Arc::new(delete));
}

type Reply = Result<(StatusCode, Value), ArmError>;

fn invalid(message: impl Into<String>) -> ArmError {
    ArmError::bad_request("InvalidRequestPropertyValue", message)
}

fn not_found(id: &str) -> ArmError {
    ArmError::not_found("ObjectReplicationPolicyNotFound", format!("The object replication policy '{id}' was not found."))
}

/// An account named by its name or by its resource id.
fn account_name(reference: &str) -> String {
    reference.trim_end_matches('/').rsplit('/').next().unwrap_or(reference).to_ascii_lowercase()
}

fn policy_json(account_id: &str, policy_id: &str, policy: &Policy) -> Value {
    json!({
        "id": format!("{account_id}/objectReplicationPolicies/{policy_id}"),
        "name": policy_id,
        "type": "Microsoft.Storage/storageAccounts/objectReplicationPolicies",
        "properties": policy.properties,
    })
}

fn list(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let account = request.param("accountName");
    let account_id = require_account(ctx, account)?;
    let state = ctx.engine.state::<StorageState>();
    let state = state.lock();
    let items: Vec<Value> = state
        .accounts
        .get(account)
        .into_iter()
        .flat_map(|a| a.replication.iter())
        .map(|(id, policy)| policy_json(&account_id, id, policy))
        .collect();
    Ok((StatusCode::OK, json!({"value": items})))
}

fn get(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let account = request.param("accountName");
    let account_id = require_account(ctx, account)?;
    let id = request.param("objectReplicationPolicyId");
    let state = ctx.engine.state::<StorageState>();
    let state = state.lock();
    let policy = state.accounts.get(account).and_then(|a| a.replication.get(id)).ok_or_else(|| not_found(id))?;
    Ok((StatusCode::OK, policy_json(&account_id, id, policy)))
}

fn delete(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let account = request.param("accountName");
    require_account(ctx, account)?;
    let state = ctx.engine.state::<StorageState>();
    let mut state = state.lock();
    let removed = state.accounts.get_mut(account).and_then(|a| a.replication.remove(request.param("objectReplicationPolicyId")));
    Ok((if removed.is_some() { StatusCode::OK } else { StatusCode::NO_CONTENT }, Value::Null))
}

/// Whether the account's blob service has `pointer` set to true.
fn blob_service_flag(ctx: &ArmContext<'_>, account_id: &str, pointer: &str) -> bool {
    let state = ctx.engine.state::<ArmState>();
    let state = state.lock();
    state
        .resource(&format!("{account_id}/blobServices/default"))
        .and_then(|service| service.properties.pointer(pointer))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn put(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let account = request.param("accountName");
    let account_id = require_account(ctx, account)?;
    let path_id = request.param("objectReplicationPolicyId");
    let given = request.body.get("properties").cloned().unwrap_or_else(|| json!({}));
    let text = |member: &str| given.get(member).and_then(Value::as_str).filter(|v| !v.is_empty());
    let source = text("sourceAccount").ok_or_else(|| invalid("'properties.sourceAccount' is required."))?.to_string();
    let destination = text("destinationAccount").ok_or_else(|| invalid("'properties.destinationAccount' is required."))?.to_string();
    let is_source = match account.to_ascii_lowercase() {
        name if name == account_name(&destination) => false,
        name if name == account_name(&source) => true,
        _ => return Err(invalid("The account must be the policy's source or destination account.")),
    };
    if path_id.eq_ignore_ascii_case("default") && is_source {
        return Err(invalid(
            "A policy is created on the destination account first; on the source account, use the policy id the destination returned.",
        ));
    }

    if !blob_service_flag(ctx, &account_id, "/isVersioningEnabled") {
        return Err(invalid("Object replication requires blob versioning to be enabled on the account."));
    }
    if is_source && !blob_service_flag(ctx, &account_id, "/changeFeed/enabled") {
        return Err(invalid("Object replication requires the change feed to be enabled on the source account."));
    }

    let state = ctx.engine.state::<StorageState>();
    let mut state = state.lock();
    let containers: Vec<String> = state.accounts.get(account).map(|a| a.containers.keys().cloned().collect()).unwrap_or_default();
    let existing = state.accounts.get(account).and_then(|a| a.replication.get(path_id));
    if !is_source && !path_id.eq_ignore_ascii_case("default") && existing.is_none() {
        return Err(not_found(path_id));
    }
    let policy_id = if path_id.eq_ignore_ascii_case("default") { uuid::Uuid::new_v4().to_string() } else { path_id.to_string() };
    let enabled_time = existing.and_then(|p| p.properties.get("enabledTime").cloned());

    let mut rules = Vec::new();
    let mut rule_values = Vec::new();
    for rule in given.get("rules").and_then(Value::as_array).into_iter().flatten() {
        let field = |member: &str| rule.get(member).and_then(Value::as_str).filter(|v| !v.is_empty());
        let source_container = field("sourceContainer").ok_or_else(|| invalid("Each rule needs a 'sourceContainer'."))?;
        let destination_container = field("destinationContainer").ok_or_else(|| invalid("Each rule needs a 'destinationContainer'."))?;
        let own_container = if is_source { source_container } else { destination_container };
        if !containers.iter().any(|c| c == own_container) {
            return Err(invalid(format!("The container '{own_container}' does not exist on this account.")));
        }
        let id = match field("ruleId") {
            Some(id) => id.to_string(),
            None if !is_source => uuid::Uuid::new_v4().to_string(),
            None => return Err(invalid("On the source account, each rule needs the 'ruleId' the destination account assigned.")),
        };
        let prefixes: Vec<String> = rule
            .pointer("/filters/prefixMatch")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        let min_creation = match rule.pointer("/filters/minCreationTime").and_then(Value::as_str) {
            None => None,
            Some(text) => Some(
                chrono::DateTime::parse_from_rfc3339(text)
                    .map_err(|_| invalid(format!("'minCreationTime' '{text}' is not a valid UTC time.")))?
                    .with_timezone(&chrono::Utc)
                    .into(),
            ),
        };
        let mut value = rule.clone();
        value["ruleId"] = json!(id);
        rule_values.push(value);
        rules.push(Rule { id, source_container: source_container.into(), destination_container: destination_container.into(), prefixes, min_creation });
    }

    let mut properties = json!({
        "policyId": policy_id,
        "sourceAccount": source,
        "destinationAccount": destination,
        "rules": rule_values,
    });
    for member in ["metrics", "priorityReplication", "tagsReplication"] {
        if let Some(value) = given.get(member).filter(|v| !v.is_null()) {
            properties[member] = value.clone();
        }
    }
    // Replication starts when the source side is in place.
    if is_source {
        properties["enabledTime"] = enabled_time.unwrap_or_else(|| json!(iso_timestamp()));
    }
    let policy = Policy { properties, destination: account_name(&destination), is_source, rules };
    let body = policy_json(&account_id, &policy_id, &policy);
    let initial: Vec<(String, String)> = if is_source { initial_copies(&state, account, &policy) } else { Vec::new() };
    state.accounts.get_mut(account).expect("account checked").replication.insert(policy_id, policy);
    for (container, blob) in initial {
        replicate(&mut state, account, &container, &blob);
    }
    Ok((StatusCode::OK, body))
}

/// The existing source blobs a newly installed policy copies: those created at
/// or after a rule's `minCreationTime`.
fn initial_copies(state: &StorageState, account: &str, policy: &Policy) -> Vec<(String, String)> {
    let Some(account) = state.accounts.get(account) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for rule in &policy.rules {
        let (Some(since), Some(container)) = (rule.min_creation, account.containers.get(&rule.source_container)) else {
            continue;
        };
        for (name, blob) in &container.blobs {
            if blob.created >= since && rule.matches(&rule.source_container, name) {
                out.push((rule.source_container.clone(), name.clone()));
            }
        }
    }
    out
}

/// Copies the blob just written to `account`/`container`/`name` to every
/// destination a policy on the account sends it to.
pub(crate) fn replicate(state: &mut StorageState, account: &str, container: &str, name: &str) {
    let targets: Vec<(String, String, String, String)> = state
        .accounts
        .get(account)
        .into_iter()
        .flat_map(|a| a.replication.iter())
        .filter(|(_, policy)| policy.is_source)
        .flat_map(|(id, policy)| {
            policy
                .rules
                .iter()
                .filter(|rule| rule.matches(container, name))
                .map(move |rule| (id.clone(), rule.id.clone(), policy.destination.clone(), rule.destination_container.clone()))
        })
        .collect();
    for (policy_id, rule_id, destination, destination_container) in targets {
        let Some(source_blob) = state.accounts.get(account).and_then(|a| a.containers.get(container)).and_then(|c| c.blobs.get(name)).cloned() else {
            return;
        };
        // The destination must have the same policy installed, and the container.
        let ready = state
            .accounts
            .get(&destination)
            .is_some_and(|d| d.replication.get(&policy_id).is_some_and(|p| !p.is_source) && d.containers.contains_key(&destination_container));
        if !ready {
            continue;
        }
        let etag = state.next_etag();
        let mut replica = source_blob;
        replica.etag = etag;
        replica.last_modified = SystemTime::now();
        replica.replicated_by.clear();
        replica.replica_of = Some(policy_id.clone());
        replica.lease = None;
        replica.copy = None;
        if let Some(target) = state.accounts.get_mut(&destination).and_then(|d| d.containers.get_mut(&destination_container)) {
            target.blobs.insert(name.to_string(), replica);
        }
        let pair = format!("{policy_id}_{rule_id}");
        if let Some(blob) = state.accounts.get_mut(account).and_then(|a| a.containers.get_mut(container)).and_then(|c| c.blobs.get_mut(name)) {
            if !blob.replicated_by.contains(&pair) {
                blob.replicated_by.push(pair);
            }
        }
    }
}
