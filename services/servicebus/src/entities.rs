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

pub(crate) struct Queues;

impl ResourceTypeHandler for Queues {
    fn resource_type(&self) -> &'static str {
        "Microsoft.ServiceBus/namespaces/queues"
    }

    fn put(&self, _ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        entity_properties(
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
        )
    }
}

pub(crate) struct Topics;

impl ResourceTypeHandler for Topics {
    fn resource_type(&self) -> &'static str {
        "Microsoft.ServiceBus/namespaces/topics"
    }

    fn put(&self, _ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        entity_properties(
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
        )
    }

    fn view(&self, ctx: &ArmContext<'_>, resource: &mut Resource) {
        let state = ctx.engine.state::<ArmState>();
        let count = state.lock().children(&format!("{}/subscriptions", resource.id)).count();
        resource.properties["subscriptionCount"] = json!(count);
    }
}

pub(crate) struct Subscriptions;

impl ResourceTypeHandler for Subscriptions {
    fn resource_type(&self) -> &'static str {
        "Microsoft.ServiceBus/namespaces/topics/subscriptions"
    }

    fn put(&self, _ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        entity_properties(
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
        )
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
            properties["action"] = action;
        }
        Ok(properties)
    }
}
