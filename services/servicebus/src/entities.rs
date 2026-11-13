//! Queues, topics, subscriptions and rules.

use emulator_core::arm::{ArmContext, ArmState, PutRequest, Resource, ResourceTypeHandler};
use emulator_core::http_util::iso_timestamp;
use emulator_core::ArmError;
use serde_json::{json, Value};

use crate::namespaces::invalid;

/// How the service renders "never": the largest .NET `TimeSpan`.
const FOREVER: &str = "P10675199DT2H48M5.4775807S";
/// What `accessedAt` reads until the entity is first used.
const NEVER_ACCESSED: &str = "0001-01-01T00:00:00Z";

fn count_details() -> Value {
    json!({
        "activeMessageCount": 0,
        "deadLetterMessageCount": 0,
        "scheduledMessageCount": 0,
        "transferMessageCount": 0,
        "transferDeadLetterMessageCount": 0,
    })
}

/// Builds an entity's properties: the stored ones (or `defaults` for a new
/// entity) overlaid with the `settable` members of the request. Members outside
/// `settable` are service-owned and ignored. A member in `fixed` cannot change
/// once the entity exists.
fn entity_properties(
    request: &PutRequest<'_>,
    defaults: Value,
    settable: &[&str],
    fixed: &[&str],
) -> Result<Value, ArmError> {
    let mut properties = match request.existing {
        Some(existing) => existing.properties.clone(),
        None => {
            let mut fresh = defaults;
            fresh["createdAt"] = json!(iso_timestamp());
            fresh["accessedAt"] = json!(NEVER_ACCESSED);
            fresh
        }
    };
    for member in settable {
        let Some(value) = request.body.pointer(&format!("/properties/{member}")).filter(|v| !v.is_null()) else {
            continue;
        };
        if request.existing.is_some() && fixed.contains(member) && properties.get(*member) != Some(value) {
            return Err(invalid(format!(
                "The value for the '{member}' property cannot be changed after the entity '{}' is created.",
                request.name
            )));
        }
        properties[*member] = value.clone();
    }
    if properties.get("maxDeliveryCount").and_then(Value::as_i64).is_some_and(|count| count < 1) {
        return Err(invalid("The value for the 'maxDeliveryCount' property must be greater than 0."));
    }
    properties["updatedAt"] = json!(iso_timestamp());
    Ok(properties)
}

/// Serves a `PATCH` with the type's own `PUT` logic over the stored entity, so an
/// update is validated the same way and cannot set service-owned properties.
fn patch_as_put(
    handler: &dyn ResourceTypeHandler,
    ctx: &ArmContext<'_>,
    existing: &Resource,
    body: &Value,
) -> Result<Value, ArmError> {
    handler.put(
        ctx,
        &PutRequest {
            id: &existing.id,
            name: &existing.name,
            location: None,
            existing: Some(existing),
            body,
        },
    )
}

/// The broker's key for an entity resource: `namespace/queue` or
/// `namespace/topic[/subscriptions/subscription]`, lowercased.
fn broker_key(resource: &Resource) -> Option<String> {
    let lower = resource.id.to_ascii_lowercase();
    let (_, rest) = lower.split_once("/providers/microsoft.servicebus/namespaces/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    Some(match parts.as_slice() {
        [ns, "queues", queue] | [ns, "topics", queue] => format!("{ns}/{queue}"),
        [ns, "topics", topic, "subscriptions", sub] => format!("{ns}/{topic}/subscriptions/{sub}"),
        [ns] => ns.to_string(),
        _ => return None,
    })
}

/// Shows the broker's message counts on a queue or subscription.
fn show_counts(ctx: &ArmContext<'_>, resource: &mut Resource) {
    let Some(key) = broker_key(resource) else { return };
    let counts = crate::broker::store::counts(ctx.engine, &key);
    resource.properties["messageCount"] = counts["messageCount"].clone();
    resource.properties["countDetails"] = counts["countDetails"].clone();
}

/// Drops the messages of a deleted entity (and of everything under it).
pub(crate) fn forget_messages(ctx: &ArmContext<'_>, resource: &Resource) {
    if let Some(key) = broker_key(resource) {
        crate::broker::store::forget(ctx.engine, &key);
    }
}

/// The tier (`Basic`, `Standard`, `Premium`) of the namespace `id` is in.
fn tier(ctx: &ArmContext<'_>, id: &str) -> String {
    let lower = id.to_ascii_lowercase();
    let Some(at) = lower.find("/namespaces/") else { return "Standard".into() };
    let rest = &id[at + "/namespaces/".len()..];
    let namespace_id = &id[..at + "/namespaces/".len() + rest.find('/').unwrap_or(rest.len())];
    let state = ctx.engine.state::<ArmState>();
    let state = state.lock();
    state
        .resource(namespace_id)
        .and_then(|ns| ns.sku.as_ref())
        .and_then(|sku| sku.get("tier").or_else(|| sku.get("name")))
        .and_then(Value::as_str)
        .unwrap_or("Standard")
        .to_string()
}

/// Refuses what the namespace's tier does not offer, as Service Bus does:
/// Basic has no topics, sessions, duplicate detection or auto-forwarding;
/// only Premium sets the maximum message size (1 MB to 100 MB, 1 MB by
/// default), which is 256 KB otherwise.
fn check_tier(ctx: &ArmContext<'_>, request: &PutRequest<'_>, properties: &mut Value, topic: bool) -> Result<(), ArmError> {
    let tier = tier(ctx, request.id);
    let refuse = |feature: &str| {
        invalid(format!(
            "SubCode=40000. {feature} is not supported in the '{tier}' tier of namespace. Use a Standard or Premium namespace."
        ))
    };
    if tier.eq_ignore_ascii_case("Basic") {
        if topic {
            return Err(refuse("A topic"));
        }
        for (member, feature) in [("requiresSession", "Sessions"), ("requiresDuplicateDetection", "Duplicate detection")] {
            if properties.get(member).and_then(Value::as_bool) == Some(true) {
                return Err(refuse(feature));
            }
        }
        for member in ["forwardTo", "forwardDeadLetteredMessagesTo"] {
            if properties.get(member).and_then(Value::as_str).is_some_and(|t| !t.is_empty()) {
                return Err(refuse("Auto-forwarding"));
            }
        }
    }
    let given = request.body.pointer("/properties/maxMessageSizeInKilobytes").and_then(Value::as_i64);
    if tier.eq_ignore_ascii_case("Premium") {
        let size = given.or_else(|| properties.get("maxMessageSizeInKilobytes").and_then(Value::as_i64)).unwrap_or(1024);
        if !(1024..=102_400).contains(&size) {
            return Err(invalid("The maxMessageSizeInKilobytes must be between 1024 and 102400."));
        }
        properties["maxMessageSizeInKilobytes"] = json!(size);
    } else if given.is_some() {
        return Err(invalid(format!(
            "SubCode=40000. The maxMessageSizeInKilobytes property can only be set on Premium namespaces; this namespace is '{tier}'."
        )));
    }
    Ok(())
}

pub(crate) struct Queues;

impl ResourceTypeHandler for Queues {
    fn resource_type(&self) -> &'static str {
        "Microsoft.ServiceBus/namespaces/queues"
    }

    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        let mut properties = entity_properties(
            request,
            json!({
                "lockDuration": "PT1M",
                "maxSizeInMegabytes": 1024,
                "requiresDuplicateDetection": false,
                "requiresSession": false,
                "defaultMessageTimeToLive": FOREVER,
                "deadLetteringOnMessageExpiration": false,
                "duplicateDetectionHistoryTimeWindow": "PT10M",
                "maxDeliveryCount": 10,
                "status": "Active",
                "enableBatchedOperations": true,
                "autoDeleteOnIdle": FOREVER,
                "enablePartitioning": false,
                "enableExpress": false,
                "messageCount": 0,
                "sizeInBytes": 0,
                "countDetails": count_details(),
            }),
            &[
                "lockDuration",
                "maxSizeInMegabytes",
                "maxMessageSizeInKilobytes",
                "requiresDuplicateDetection",
                "requiresSession",
                "defaultMessageTimeToLive",
                "deadLetteringOnMessageExpiration",
                "duplicateDetectionHistoryTimeWindow",
                "maxDeliveryCount",
                "status",
                "enableBatchedOperations",
                "autoDeleteOnIdle",
                "enablePartitioning",
                "enableExpress",
                "forwardTo",
                "forwardDeadLetteredMessagesTo",
                "userMetadata",
            ],
            &["requiresDuplicateDetection", "requiresSession", "enablePartitioning"],
        )?;
        check_tier(ctx, request, &mut properties, false)?;
        Ok(properties)
    }

    fn patch(&self, ctx: &ArmContext<'_>, existing: &Resource, body: &Value) -> Result<Value, ArmError> {
        patch_as_put(self, ctx, existing, body)
    }

    fn view(&self, ctx: &ArmContext<'_>, resource: &mut Resource) {
        show_counts(ctx, resource);
    }

    fn deleted(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        forget_messages(ctx, resource);
    }
}

pub(crate) struct Topics;

impl ResourceTypeHandler for Topics {
    fn resource_type(&self) -> &'static str {
        "Microsoft.ServiceBus/namespaces/topics"
    }

    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        let mut properties = entity_properties(
            request,
            json!({
                "defaultMessageTimeToLive": FOREVER,
                "maxSizeInMegabytes": 1024,
                "requiresDuplicateDetection": false,
                "duplicateDetectionHistoryTimeWindow": "PT10M",
                "enableBatchedOperations": true,
                "status": "Active",
                "supportOrdering": true,
                "autoDeleteOnIdle": FOREVER,
                "enablePartitioning": false,
                "enableExpress": false,
                "sizeInBytes": 0,
                "subscriptionCount": 0,
                "countDetails": count_details(),
            }),
            &[
                "defaultMessageTimeToLive",
                "maxSizeInMegabytes",
                "maxMessageSizeInKilobytes",
                "requiresDuplicateDetection",
                "duplicateDetectionHistoryTimeWindow",
                "enableBatchedOperations",
                "status",
                "supportOrdering",
                "autoDeleteOnIdle",
                "enablePartitioning",
                "enableExpress",
                "userMetadata",
            ],
            &["requiresDuplicateDetection", "enablePartitioning"],
        )?;
        check_tier(ctx, request, &mut properties, true)?;
        Ok(properties)
    }

    fn patch(&self, ctx: &ArmContext<'_>, existing: &Resource, body: &Value) -> Result<Value, ArmError> {
        patch_as_put(self, ctx, existing, body)
    }

    fn view(&self, ctx: &ArmContext<'_>, resource: &mut Resource) {
        let state = ctx.engine.state::<ArmState>();
        let count = state.lock().children(&format!("{}/subscriptions", resource.id)).count();
        resource.properties["subscriptionCount"] = json!(count);
    }

    fn deleted(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        forget_messages(ctx, resource);
    }
}

pub(crate) struct Subscriptions;

impl ResourceTypeHandler for Subscriptions {
    fn resource_type(&self) -> &'static str {
        "Microsoft.ServiceBus/namespaces/topics/subscriptions"
    }

    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        let mut properties = entity_properties(
            request,
            json!({
                "lockDuration": "PT1M",
                "requiresSession": false,
                "defaultMessageTimeToLive": FOREVER,
                "deadLetteringOnFilterEvaluationExceptions": true,
                "deadLetteringOnMessageExpiration": false,
                "maxDeliveryCount": 10,
                "status": "Active",
                "enableBatchedOperations": true,
                "autoDeleteOnIdle": FOREVER,
                "isClientAffine": false,
                "messageCount": 0,
                "countDetails": count_details(),
            }),
            &[
                "lockDuration",
                "requiresSession",
                "defaultMessageTimeToLive",
                "deadLetteringOnFilterEvaluationExceptions",
                "deadLetteringOnMessageExpiration",
                "duplicateDetectionHistoryTimeWindow",
                "maxDeliveryCount",
                "status",
                "enableBatchedOperations",
                "autoDeleteOnIdle",
                "forwardTo",
                "forwardDeadLetteredMessagesTo",
                "isClientAffine",
                "clientAffineProperties",
                "userMetadata",
            ],
            &["requiresSession"],
        )?;
        check_tier(ctx, request, &mut properties, false)?;
        Ok(properties)
    }

    fn patch(&self, ctx: &ArmContext<'_>, existing: &Resource, body: &Value) -> Result<Value, ArmError> {
        patch_as_put(self, ctx, existing, body)
    }

    fn view(&self, ctx: &ArmContext<'_>, resource: &mut Resource) {
        show_counts(ctx, resource);
    }

    fn deleted(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        forget_messages(ctx, resource);
    }

    fn created(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        // A new subscription receives everything until its rules say otherwise.
        ctx.engine.state::<ArmState>().lock().insert(Resource {
            id: format!("{}/rules/$Default", resource.id),
            name: "$Default".into(),
            resource_type: "Microsoft.ServiceBus/namespaces/topics/subscriptions/rules".into(),
            location: None,
            tags: None,
            kind: None,
            sku: None,
            identity: None,
            properties: json!({
                "action": {},
                "filterType": "SqlFilter",
                "sqlFilter": {"sqlExpression": "1=1", "compatibilityLevel": 20},
            }),
        });
    }
}

pub(crate) struct Rules;

impl ResourceTypeHandler for Rules {
    fn resource_type(&self) -> &'static str {
        "Microsoft.ServiceBus/namespaces/topics/subscriptions/rules"
    }

    fn put(&self, _ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        let given = |member: &str| {
            request
                .body
                .pointer(&format!("/properties/{member}"))
                .filter(|v| !v.is_null())
                .cloned()
        };
        let filter_type = given("filterType").unwrap_or_else(|| json!("SqlFilter"));
        let mut properties = json!({"filterType": filter_type, "action": {}});
        match filter_type.as_str() {
            Some("SqlFilter") => {
                // A rule with no filter matches everything.
                let mut filter = given("sqlFilter").unwrap_or_else(|| json!({"sqlExpression": "1=1"}));
                if filter.get("sqlExpression").and_then(Value::as_str).is_none_or(str::is_empty) {
                    return Err(invalid("The 'sqlExpression' of a SQL filter must not be empty."));
                }
                if filter.get("compatibilityLevel").is_none() {
                    filter["compatibilityLevel"] = json!(20);
                }
                let expression = filter["sqlExpression"].as_str().unwrap_or_default();
                crate::broker::sql::Filter::parse(expression)
                    .map_err(|e| invalid(format!("The SQL filter '{expression}' is not valid: {e}")))?;
                properties["sqlFilter"] = filter;
            }
            Some("CorrelationFilter") => {
                let filter = given("correlationFilter")
                    .filter(|f| f.as_object().is_some_and(|members| !members.is_empty()))
                    .ok_or_else(|| invalid("A correlation filter must set at least one property."))?;
                properties["correlationFilter"] = filter;
            }
            _ => return Err(invalid("The filterType must be SqlFilter or CorrelationFilter.")),
        }
        if let Some(mut action) = given("action") {
            if action.get("sqlExpression").is_some() && action.get("compatibilityLevel").is_none() {
                action["compatibilityLevel"] = json!(20);
            }
            if let Some(text) = action.get("sqlExpression").and_then(Value::as_str).filter(|t| !t.trim().is_empty()) {
                crate::broker::sql::Action::parse(text)
                    .map_err(|e| invalid(format!("The rule action '{text}' is not valid: {e}")))?;
            }
            properties["action"] = action;
        }
        Ok(properties)
    }

    fn patch(&self, ctx: &ArmContext<'_>, existing: &Resource, body: &Value) -> Result<Value, ArmError> {
        patch_as_put(self, ctx, existing, body)
    }
}
