//! Managed identities on resources: what Resource Manager fills in when a
//! resource is created or updated with an `identity` block.
//!
//! A system-assigned identity gets a principal id (kept for as long as the
//! identity stays enabled) and the tenant id. Each user-assigned identity is
//! looked up and filled in with its principal and client ids; one that does not
//! exist is refused.

use serde_json::{json, Map, Value};

use crate::arm::ArmState;
use crate::ArmError;

const USER_ASSIGNED_TYPE: &str = "Microsoft.ManagedIdentity/userAssignedIdentities";

fn invalid(message: impl Into<String>) -> ArmError {
    ArmError::bad_request("InvalidIdentity", message)
}

/// Which kinds of identity an `identity.type` value names.
fn kinds(identity: &Value) -> Result<(bool, bool), ArmError> {
    let text = identity.get("type").and_then(Value::as_str).unwrap_or("None");
    let (mut system, mut user, mut none) = (false, false, false);
    for part in text.split(',').map(|p| p.trim().to_ascii_lowercase()) {
        match part.as_str() {
            "systemassigned" => system = true,
            "userassigned" => user = true,
            "none" => none = true,
            _ => return Err(invalid(format!("The identity type '{text}' is invalid. It must be SystemAssigned, UserAssigned, 'SystemAssigned, UserAssigned' or None."))),
        }
    }
    if none && (system || user) {
        return Err(invalid("The identity type None cannot be combined with another type."));
    }
    Ok((system, user))
}

/// The identity block to store for a resource. `requested` is the request's
/// `identity`; `existing` the stored one; `merge` is true for a `PATCH`, where
/// user-assigned identities are added to the stored ones and a `null` removes one.
pub fn resolve(
    state: &mut ArmState,
    requested: Option<&Value>,
    existing: Option<&Value>,
    tenant: &str,
    merge: bool,
) -> Result<Option<Value>, ArmError> {
    let Some(requested) = requested.filter(|v| !v.is_null()) else {
        return Ok(if merge { existing.cloned() } else { None });
    };
    let (system, user) = kinds(requested)?;
    let mut out = Map::new();
    out.insert(
        "type".into(),
        json!(match (system, user) {
            (true, true) => "SystemAssigned, UserAssigned",
            (true, false) => "SystemAssigned",
            (false, true) => "UserAssigned",
            (false, false) => "None",
        }),
    );

    if system {
        let kept = existing
            .filter(|e| kinds(e).is_ok_and(|(had_system, _)| had_system))
            .and_then(|e| e.get("principalId"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let principal = match kept {
            Some(principal) => principal,
            None => {
                let principal = uuid::Uuid::new_v4().to_string();
                state.system_identity_clients.insert(principal.clone(), uuid::Uuid::new_v4().to_string());
                principal
            }
        };
        out.insert("principalId".into(), json!(principal));
        out.insert("tenantId".into(), json!(tenant));
    }

    if user {
        let mut chosen: Map<String, Value> = match (merge, existing) {
            (true, Some(existing)) => existing
                .get("userAssignedIdentities")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            _ => Map::new(),
        };
        for (id, value) in requested.get("userAssignedIdentities").and_then(Value::as_object).into_iter().flatten() {
            if value.is_null() {
                chosen.retain(|kept, _| !kept.eq_ignore_ascii_case(id));
                continue;
            }
            let identity = state
                .resource(id)
                .filter(|r| r.resource_type.eq_ignore_ascii_case(USER_ASSIGNED_TYPE))
                .ok_or_else(|| invalid(format!("The user-assigned identity '{id}' was not found.")))?;
            chosen.retain(|kept, _| !kept.eq_ignore_ascii_case(id));
            chosen.insert(
                identity.id.clone(),
                json!({
                    "principalId": identity.properties.get("principalId"),
                    "clientId": identity.properties.get("clientId"),
                }),
            );
        }
        if chosen.is_empty() {
            return Err(invalid("A UserAssigned identity must name at least one user-assigned identity."));
        }
        out.insert("userAssignedIdentities".into(), Value::Object(chosen));
    }
    Ok(Some(Value::Object(out)))
}
