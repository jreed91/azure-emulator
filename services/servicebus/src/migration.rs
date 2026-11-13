//! Standard-to-Premium migration configurations, the `POST` forms of the
//! authorization-rule reads, and the legacy event-hub listings.
//!
//! Migration is modeled up to completion: creating the configuration validates
//! the pair and copies the Standard namespace's entities (queues, topics,
//! subscriptions and rules) to the Premium one, and `revert` abandons it.
//! Completing a migration (`upgrade`) moves the Standard namespace's DNS name to
//! the Premium namespace; that hand-over is not modeled, so it answers `501`.

use std::sync::Arc;

use emulator_core::arm::{
    ArmContext, ArmRegistry, ArmState, OperationRequest, PutRequest, Resource, ResourceTypeHandler,
};
use emulator_core::ArmError;
use http::StatusCode;
use serde_json::{json, Value};

use crate::namespaces::{invalid, is_valid_namespace_name};
use crate::ServiceBusState;

const MIGRATION_TYPE: &str = "Microsoft.ServiceBus/namespaces/migrationConfigurations";

pub(crate) fn register(registry: &mut ArmRegistry) {
    registry.register_type(MigrationConfigs);
    registry.register_type(NoEventHubs("Microsoft.ServiceBus/namespaces/eventhubs"));
    registry.register_type(NoEventHubs("Microsoft.ServiceBus/namespaces/eventhubs/authorizationRules"));
    registry.register_operation("Namespaces_ListPostAuthorizationRules", Arc::new(post_list_rules));
    for id in ["Namespaces_PostAuthorizationRule", "Queues_PostAuthorizationRule", "Topics_PostAuthorizationRule"] {
        registry.register_operation(id, Arc::new(post_get_rule));
    }
}

type Reply = Result<(StatusCode, Value), ArmError>;

fn namespace_id(ctx: &ArmContext<'_>, name: &str) -> String {
    format!(
        "/subscriptions/{}/resourceGroups/{}/providers/Microsoft.ServiceBus/namespaces/{name}",
        ctx.subscription_id, ctx.resource_group
    )
}

fn not_found(kind: &str, name: &str) -> ArmError {
    ArmError::not_found("ResourceNotFound", format!("The {kind} '{name}' was not found."))
}

/// `POST .../AuthorizationRules`: the namespace's rules, as the `GET` returns them.
fn post_list_rules(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let namespace = request.param("namespaceName");
    let id = namespace_id(ctx, namespace);
    let state = ctx.engine.state::<ArmState>();
    let state = state.lock();
    if state.resource(&id).is_none() {
        return Err(not_found("namespace", namespace));
    }
    let rules: Vec<Value> = state.children(&format!("{id}/AuthorizationRules")).map(Resource::to_json).collect();
    Ok((StatusCode::OK, json!({"value": rules})))
}

/// `POST` on one rule, at namespace, queue or topic level: the rule itself.
fn post_get_rule(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let mut id = namespace_id(ctx, request.param("namespaceName"));
    for (collection, parameter) in [("queues", "queueName"), ("topics", "topicName")] {
        if let Some(entity) = request.params.get(parameter) {
            id.push_str(&format!("/{collection}/{entity}"));
        }
    }
    let rule = request.param("authorizationRuleName");
    id.push_str(&format!("/AuthorizationRules/{rule}"));
    let state = ctx.engine.state::<ArmState>();
    let state = state.lock();
    state
        .resource(&id)
        .map(|found| (StatusCode::OK, found.to_json()))
        .ok_or_else(|| not_found("authorization rule", rule))
}

/// Event hubs inside a Service Bus namespace: a legacy shape the emulator's
/// namespaces never contain, and the specification offers no way to create one.
struct NoEventHubs(&'static str);

impl ResourceTypeHandler for NoEventHubs {
    fn resource_type(&self) -> &'static str {
        self.0
    }

    fn put(&self, _ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        Err(not_found("event hub", request.name))
    }
}

struct MigrationConfigs;

/// The Service Bus entities under `namespace_id`, deepest last, so that copying
/// them in order always creates a parent before its children.
fn entities(state: &ArmState, namespace_id: &str) -> Vec<Resource> {
    let mut out = Vec::new();
    for collection in ["queues", "topics"] {
        for entity in state.children(&format!("{namespace_id}/{collection}")) {
            out.push(entity.clone());
            if collection == "topics" {
                for subscription in state.children(&format!("{}/subscriptions", entity.id)) {
                    out.push(subscription.clone());
                    out.extend(state.children(&format!("{}/rules", subscription.id)).cloned());
                }
            }
        }
    }
    out
}

fn tier(namespace: &Resource) -> &str {
    namespace.sku.as_ref().and_then(|sku| sku.get("name")).and_then(Value::as_str).unwrap_or("")
}

fn source_id(config_id: &str) -> &str {
    config_id
        .rfind("/migrationConfigurations/")
        .map(|at| &config_id[..at])
        .unwrap_or(config_id)
}

impl ResourceTypeHandler for MigrationConfigs {
    fn resource_type(&self) -> &'static str {
        MIGRATION_TYPE
    }

    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        if request.name != "$default" {
            return Err(invalid("The migration configuration name must be '$default'."));
        }
        if request.existing.is_some() {
            return Err(ArmError::conflict("Conflict", "A migration is already in progress for this namespace."));
        }
        let given = |member: &str| {
            request
                .body
                .pointer(&format!("/properties/{member}"))
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
        };
        let target_id = given("targetNamespace")
            .ok_or_else(|| invalid("The request content was invalid: the required property 'properties.targetNamespace' is missing."))?;
        let post_name = given("postMigrationName")
            .ok_or_else(|| invalid("The request content was invalid: the required property 'properties.postMigrationName' is missing."))?;
        if !is_valid_namespace_name(post_name) {
            return Err(invalid("'properties.postMigrationName' must follow the namespace name rules."));
        }
        if ctx.engine.state::<ServiceBusState>().lock().namespaces.contains(&post_name.to_ascii_lowercase()) {
            return Err(ArmError::conflict("Conflict", format!("The name '{post_name}' is already in use.")));
        }

        let state = ctx.engine.state::<ArmState>();
        let state = state.lock();
        let source = state
            .resource(source_id(request.id))
            .ok_or_else(|| invalid("The namespace being migrated does not exist."))?;
        let target = state
            .resource(target_id)
            .filter(|t| t.resource_type.eq_ignore_ascii_case("Microsoft.ServiceBus/namespaces"))
            .ok_or_else(|| invalid(format!("The target namespace '{target_id}' does not exist.")))?;
        if tier(source) != "Standard" || tier(target) != "Premium" {
            return Err(invalid("Migration is from a Standard namespace to a Premium namespace."));
        }
        if !entities(&state, &target.id).is_empty() {
            return Err(invalid("The target Premium namespace must not contain any queues or topics."));
        }
        Ok(json!({
            "provisioningState": "Succeeded",
            "targetNamespace": target.id,
            "postMigrationName": post_name,
            "migrationState": "Active",
            "pendingReplicationOperationsCount": 0,
        }))
    }

    fn created(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        // Starting a migration copies the entities' metadata to the target.
        let source = source_id(&resource.id).to_string();
        let target = resource.properties["targetNamespace"].as_str().unwrap_or("").to_string();
        let state = ctx.engine.state::<ArmState>();
        let mut state = state.lock();
        let Some(target_namespace) = state.resource(&target).map(|t| t.id.clone()) else {
            return;
        };
        for entity in entities(&state, &source) {
            let suffix = entity.id[source.len()..].to_string();
            state.insert(Resource {
                id: format!("{target_namespace}{suffix}"),
                ..entity
            });
        }
    }

    fn actions(&self) -> &'static [&'static str] {
        &["revert"]
    }

    fn action(&self, ctx: &ArmContext<'_>, resource: &Resource, action: &str, _body: &Value) -> Result<Value, ArmError> {
        match action.to_ascii_lowercase().as_str() {
            // Abandoning the migration leaves both namespaces as they are now.
            "revert" => {
                ctx.engine.state::<ArmState>().lock().remove(&resource.id);
                Ok(Value::Null)
            }
            other => Err(ArmError::not_implemented(format_args!(
                "the '{other}' action on {MIGRATION_TYPE} (completing a migration moves DNS names, which the emulator does not model)"
            ))),
        }
    }
}
