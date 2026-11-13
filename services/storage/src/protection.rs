//! Blob container protection through Resource Manager: legal holds, time-based
//! retention (immutability) policies, and container leases.
//!
//! The protections are enforced, not just recorded: while a legal hold is set,
//! or a blob is younger than the retention period, the Blob data plane refuses to
//! overwrite or delete it, and neither plane deletes a container that holds a
//! protected blob. A leased container is deleted only with its lease id.
//!
//! Not checked against Azure: the codes and wording of refusals, and the reply
//! to reading a policy on a container that has none.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use emulator_core::arm::{ArmContext, ArmRegistry, OperationRequest};
use emulator_core::http_util::iso_timestamp;
use emulator_core::ArmError;
use http::StatusCode;
use serde_json::{json, Value};

use crate::management::{container_not_found, require_account};
use crate::state::{lease_state, Container, ImmutabilityPolicy, Lease, LeaseState, LegalHoldTag, Protection};
use crate::StorageState;

type Reply = Result<(StatusCode, Value), ArmError>;

pub(crate) fn register(registry: &mut ArmRegistry) {
    registry.register_operation("BlobContainers_CreateOrUpdateImmutabilityPolicy", Arc::new(put_policy));
    registry.register_operation("BlobContainers_GetImmutabilityPolicy", Arc::new(get_policy));
    registry.register_operation("BlobContainers_DeleteImmutabilityPolicy", Arc::new(delete_policy));
    registry.register_operation("BlobContainers_LockImmutabilityPolicy", Arc::new(lock_policy));
    registry.register_operation("BlobContainers_ExtendImmutabilityPolicy", Arc::new(extend_policy));
    registry.register_operation("BlobContainers_SetLegalHold", Arc::new(set_legal_hold));
    registry.register_operation("BlobContainers_ClearLegalHold", Arc::new(clear_legal_hold));
    registry.register_operation("BlobContainers_Lease", Arc::new(lease));
}

fn invalid(message: impl Into<String>) -> ArmError {
    ArmError::bad_request("InvalidRequestPropertyValue", message)
}

/// The refusal for deleting something a protection covers.
pub(crate) fn protected(protection: Protection) -> (&'static str, &'static str) {
    match protection {
        Protection::LegalHold => (
            "ContainerProtectedFromDeletion",
            "The requested operation is not allowed as the container has a legal hold.",
        ),
        Protection::Retention => (
            "ContainerProtectedFromDeletion",
            "The requested operation is not allowed as the container holds blobs under an immutability policy.",
        ),
    }
}

/// Runs `f` on the container the request addresses, with the account checked.
fn with_container<T>(
    ctx: &ArmContext<'_>,
    request: &OperationRequest<'_>,
    f: impl FnOnce(&mut Container, &mut dyn FnMut() -> String) -> Result<T, ArmError>,
) -> Result<T, ArmError> {
    let account = request.param("accountName");
    require_account(ctx, account)?;
    let state = ctx.engine.state::<StorageState>();
    let mut state = state.lock();
    // Entity tags are minted from the store's sequence, so take it out first.
    let mut tags: Vec<String> = (0..2).map(|_| state.next_etag()).collect();
    let container = state
        .accounts
        .get_mut(account)
        .and_then(|a| a.containers.get_mut(request.param("containerName")))
        .ok_or_else(container_not_found)?;
    let mut next_etag = move || tags.pop().unwrap_or_default();
    f(container, &mut next_etag)
}

// ---- immutability policies ------------------------------------------------------

fn policy_json(ctx: &ArmContext<'_>, request: &OperationRequest<'_>, policy: &ImmutabilityPolicy) -> Value {
    let container_id = format!(
        "/subscriptions/{}/resourceGroups/{}/providers/Microsoft.Storage/storageAccounts/{}/blobServices/default/containers/{}",
        ctx.subscription_id,
        ctx.resource_group,
        request.param("accountName"),
        request.param("containerName")
    );
    let mut properties = json!({
        "immutabilityPeriodSinceCreationInDays": policy.days,
        "state": if policy.locked { "Locked" } else { "Unlocked" },
    });
    if let Some(allow) = policy.allow_protected_append_writes {
        properties["allowProtectedAppendWrites"] = json!(allow);
    }
    if let Some(allow) = policy.allow_protected_append_writes_all {
        properties["allowProtectedAppendWritesAll"] = json!(allow);
    }
    json!({
        "id": format!("{container_id}/immutabilityPolicies/default"),
        "name": "default",
        "type": "Microsoft.Storage/storageAccounts/blobServices/containers/immutabilityPolicies",
        "etag": policy.etag,
        "properties": properties,
    })
}

/// Embedded in the container's own representation.
pub(crate) fn container_policy_json(policy: &ImmutabilityPolicy) -> Value {
    let history: Vec<Value> = policy
        .history
        .iter()
        .map(|(update, days, timestamp)| json!({"update": update, "immutabilityPeriodSinceCreationInDays": days, "timestamp": timestamp}))
        .collect();
    json!({
        "etag": policy.etag,
        "properties": {
            "immutabilityPeriodSinceCreationInDays": policy.days,
            "state": if policy.locked { "Locked" } else { "Unlocked" },
        },
        "updateHistory": history,
    })
}

/// The policy name in the path; `default` is the only one.
fn check_policy_name(request: &OperationRequest<'_>) -> Result<(), ArmError> {
    match request.params.get("immutabilityPolicyName").map(String::as_str) {
        None | Some("default") => Ok(()),
        Some(other) => Err(invalid(format!("The immutability policy name '{other}' is invalid; the only name is 'default'."))),
    }
}

/// Checks `If-Match` against the policy's entity tag. `required` operations
/// refuse a request without one.
fn check_if_match(request: &OperationRequest<'_>, policy: &ImmutabilityPolicy, required: bool) -> Result<(), ArmError> {
    match request.header("if-match") {
        None if required => Err(ArmError::bad_request(
            "MissingRequiredHeader",
            "The If-Match header is required for this operation.",
        )),
        None | Some("*") => Ok(()),
        Some(sent) if sent == policy.etag => Ok(()),
        Some(_) => Err(ArmError::new(
            StatusCode::PRECONDITION_FAILED,
            "ConditionNotMet",
            "The condition specified using HTTP conditional header(s) is not met.",
        )),
    }
}

fn no_policy() -> ArmError {
    ArmError::not_found(
        "ImmutabilityPolicyNotFound",
        "The container does not have an immutability policy.",
    )
}

fn requested_days(request: &OperationRequest<'_>) -> Result<i64, ArmError> {
    request
        .body
        .pointer("/properties/immutabilityPeriodSinceCreationInDays")
        .and_then(Value::as_i64)
        .filter(|days| (1..=146_000).contains(days))
        .ok_or_else(|| invalid("'properties.immutabilityPeriodSinceCreationInDays' must be between 1 and 146000."))
}

fn put_policy(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    check_policy_name(request)?;
    let days = requested_days(request)?;
    let flag = |member: &str| request.body.pointer(&format!("/properties/{member}")).and_then(Value::as_bool);
    let policy = with_container(ctx, request, |container, next_etag| {
        if let Some(existing) = &container.immutability {
            if existing.locked {
                return Err(ArmError::conflict(
                    "ImmutabilityPolicyLocked",
                    "A locked immutability policy can only be extended.",
                ));
            }
            check_if_match(request, existing, false)?;
        }
        let mut history = container.immutability.take().map(|p| p.history).unwrap_or_default();
        history.push(("put", days, iso_timestamp()));
        let policy = ImmutabilityPolicy {
            days,
            locked: false,
            allow_protected_append_writes: flag("allowProtectedAppendWrites"),
            allow_protected_append_writes_all: flag("allowProtectedAppendWritesAll"),
            etag: next_etag(),
            history,
        };
        let out = policy_json(ctx, request, &policy);
        container.immutability = Some(policy);
        Ok(out)
    })?;
    Ok((StatusCode::OK, policy))
}

fn get_policy(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    check_policy_name(request)?;
    let policy = with_container(ctx, request, |container, _| {
        container.immutability.as_ref().map(|p| policy_json(ctx, request, p)).ok_or_else(no_policy)
    })?;
    Ok((StatusCode::OK, policy))
}

fn delete_policy(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    check_policy_name(request)?;
    let policy = with_container(ctx, request, |container, _| {
        let existing = container.immutability.as_ref().ok_or_else(no_policy)?;
        check_if_match(request, existing, true)?;
        if existing.locked {
            return Err(ArmError::conflict(
                "ImmutabilityPolicyLocked",
                "A locked immutability policy cannot be deleted.",
            ));
        }
        let removed = container.immutability.take().expect("checked above");
        Ok(policy_json(ctx, request, &removed))
    })?;
    Ok((StatusCode::OK, policy))
}

fn lock_policy(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let policy = with_container(ctx, request, |container, next_etag| {
        let policy = container.immutability.as_mut().ok_or_else(no_policy)?;
        check_if_match(request, policy, true)?;
        if !policy.locked {
            policy.locked = true;
            policy.etag = next_etag();
            policy.history.push(("lock", policy.days, iso_timestamp()));
        }
        Ok(policy_json(ctx, request, policy))
    })?;
    Ok((StatusCode::OK, policy))
}

fn extend_policy(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let days = requested_days(request)?;
    let policy = with_container(ctx, request, |container, next_etag| {
        let policy = container.immutability.as_mut().ok_or_else(no_policy)?;
        check_if_match(request, policy, true)?;
        if !policy.locked {
            return Err(ArmError::conflict(
                "ImmutabilityPolicyNotLocked",
                "Only a locked immutability policy can be extended; update an unlocked one instead.",
            ));
        }
        if days < policy.days {
            return Err(invalid("A locked immutability policy's retention period can only be increased."));
        }
        policy.days = days;
        policy.etag = next_etag();
        policy.history.push(("extend", days, iso_timestamp()));
        Ok(policy_json(ctx, request, policy))
    })?;
    Ok((StatusCode::OK, policy))
}

// ---- legal holds ------------------------------------------------------------------

fn requested_tags(request: &OperationRequest<'_>) -> Result<Vec<String>, ArmError> {
    let tags = request
        .body
        .get("tags")
        .and_then(Value::as_array)
        .filter(|tags| !tags.is_empty())
        .ok_or_else(|| invalid("'tags' must list at least one tag."))?;
    tags.iter()
        .map(|tag| {
            let tag = tag.as_str().unwrap_or("");
            let valid = (3..=23).contains(&tag.len()) && tag.bytes().all(|b| b.is_ascii_alphanumeric());
            // Tags are compared and stored in lower case.
            valid
                .then(|| tag.to_ascii_lowercase())
                .ok_or_else(|| invalid(format!("The legal hold tag '{tag}' is invalid. It must be 3 to 23 letters and digits.")))
        })
        .collect()
}

pub(crate) fn legal_hold_json(container: &Container) -> Value {
    json!({
        "hasLegalHold": !container.legal_hold.is_empty(),
        "tags": container.legal_hold.iter().map(|t| json!({"tag": t.tag, "timestamp": t.timestamp})).collect::<Vec<_>>(),
    })
}

fn legal_hold_reply(container: &Container) -> Value {
    json!({
        "hasLegalHold": !container.legal_hold.is_empty(),
        "tags": container.legal_hold.iter().map(|t| t.tag.clone()).collect::<Vec<_>>(),
    })
}

fn set_legal_hold(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let tags = requested_tags(request)?;
    let reply = with_container(ctx, request, |container, _| {
        for tag in tags {
            if !container.legal_hold.iter().any(|existing| existing.tag == tag) {
                container.legal_hold.push(LegalHoldTag { tag, timestamp: iso_timestamp() });
            }
        }
        if container.legal_hold.len() > 10 {
            container.legal_hold.truncate(10);
            return Err(invalid("A container can hold at most 10 legal hold tags."));
        }
        Ok(legal_hold_reply(container))
    })?;
    Ok((StatusCode::OK, reply))
}

fn clear_legal_hold(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let tags = requested_tags(request)?;
    let reply = with_container(ctx, request, |container, _| {
        container.legal_hold.retain(|existing| !tags.contains(&existing.tag));
        Ok(legal_hold_reply(container))
    })?;
    Ok((StatusCode::OK, reply))
}

// ---- leases -------------------------------------------------------------------------

fn lease_conflict(message: &str) -> ArmError {
    ArmError::conflict("LeaseAlreadyPresent", message)
}

fn lease(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let reply = with_container(ctx, request, |container, _| apply_lease_action(&mut container.lease, request.body, "container"))?;
    Ok((StatusCode::OK, reply))
}

/// Applies a lease request (`Acquire`, `Renew`, `Change`, `Release`, `Break`)
/// to `slot`, the lease of a container or file share, and returns the reply.
pub(crate) fn apply_lease_action(slot: &mut Option<Lease>, body: &Value, what: &str) -> Result<Value, ArmError> {
    let text = |member: &str| body.get(member).and_then(Value::as_str).filter(|v| !v.is_empty());
    let action = text("action").unwrap_or("").to_ascii_lowercase();
    let now = SystemTime::now();
    let state = lease_state(slot.as_ref(), now);
    let current_id = slot.as_ref().map(|l| l.id.clone());
    let matches_current = |id: Option<&str>| id.is_some_and(|id| current_id.as_deref() == Some(id));
    let need_id = || text("leaseId").ok_or_else(|| invalid("'leaseId' is required for this lease action."));
    let mismatch = || lease_conflict(&format!("The lease ID specified did not match the lease ID for the {what}."));
    match action.as_str() {
        "acquire" => {
            let duration = match body.get("leaseDuration").and_then(Value::as_i64).unwrap_or(-1) {
                -1 => None,
                seconds @ 15..=60 => Some(seconds as u64),
                _ => return Err(invalid("'leaseDuration' must be -1 (infinite) or between 15 and 60 seconds.")),
            };
            let proposed = text("proposedLeaseId");
            if state.blocks() && !matches_current(proposed) {
                return Err(lease_conflict("There is already a lease present."));
            }
            let id = proposed.map(str::to_string).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            *slot = Some(Lease { id: id.clone(), duration, acquired: now, broken_at: None });
            Ok(json!({"leaseId": id}))
        }
        "renew" => {
            let id = need_id()?;
            if !matches_current(Some(id)) || matches!(state, LeaseState::Breaking | LeaseState::Broken) {
                return Err(mismatch());
            }
            slot.as_mut().expect("matched").acquired = now;
            Ok(json!({"leaseId": id}))
        }
        "change" => {
            let id = need_id()?;
            let proposed = text("proposedLeaseId").ok_or_else(|| invalid("'proposedLeaseId' is required to change a lease."))?;
            if !matches_current(Some(id)) || !state.blocks() {
                return Err(mismatch());
            }
            slot.as_mut().expect("matched").id = proposed.to_string();
            Ok(json!({"leaseId": proposed}))
        }
        "release" => {
            if !matches_current(Some(need_id()?)) {
                return Err(mismatch());
            }
            *slot = None;
            Ok(json!({}))
        }
        "break" => {
            let lease = match (state, slot.as_mut()) {
                (LeaseState::Leased | LeaseState::Breaking, Some(lease)) => lease,
                _ => {
                    return Err(ArmError::conflict(
                        "LeaseNotPresentWithLeaseOperation",
                        format!("There is currently no lease on the {what}."),
                    ))
                }
            };
            let remaining = match lease.duration {
                None => 0,
                Some(seconds) => (lease.acquired + Duration::from_secs(seconds))
                    .duration_since(now)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            };
            let period = match body.get("breakPeriod").and_then(Value::as_i64) {
                None => remaining,
                Some(seconds @ 0..=60) => seconds as u64,
                Some(_) => return Err(invalid("'breakPeriod' must be between 0 and 60 seconds.")),
            };
            // A finite lease breaks no later than it would have expired.
            let period = if lease.duration.is_some() { period.min(remaining) } else { period };
            let breaks_at = now + Duration::from_secs(period);
            // A break already under way only ever comes sooner.
            lease.broken_at = Some(lease.broken_at.map_or(breaks_at, |at| at.min(breaks_at)));
            let left = lease.broken_at.expect("set").duration_since(now).map(|d| d.as_secs()).unwrap_or(0);
            Ok(json!({"leaseTimeSeconds": left.to_string()}))
        }
        _ => Err(invalid("'action' must be Acquire, Renew, Change, Release or Break.")),
    }
}
