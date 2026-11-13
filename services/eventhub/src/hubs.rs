//! Event hubs and their consumer groups.

use emulator_core::arm::{ArmContext, ArmState, PutRequest, Resource, ResourceTypeHandler};
use emulator_core::http_util::iso_timestamp;
use emulator_core::ArmError;
use serde_json::{json, Value};

const HUB_TYPE: &str = "Microsoft.EventHub/namespaces/eventhubs";
const GROUP_TYPE: &str = "Microsoft.EventHub/namespaces/eventhubs/consumergroups";

fn invalid(message: impl Into<String>) -> ArmError {
    ArmError::bad_request("BadRequest", message)
}

/// The tier of the namespace `id` is in.
fn tier(ctx: &ArmContext<'_>, id: &str) -> String {
    let lower = id.to_ascii_lowercase();
    let Some(at) = lower.find("/namespaces/") else { return "Standard".into() };
    let rest = &id[at + "/namespaces/".len()..];
    let namespace_id = &id[..at + "/namespaces/".len() + rest.find('/').unwrap_or(rest.len())];
    let state = ctx.engine.state::<ArmState>();
    let state = state.lock();
    let tier = state
        .resource(namespace_id)
        .and_then(|ns| ns.sku.as_ref())
        .and_then(|sku| sku.get("tier").or_else(|| sku.get("name")))
        .and_then(Value::as_str)
        .unwrap_or("Standard")
        .to_string();
    tier
}

pub(crate) struct EventHubs;

impl ResourceTypeHandler for EventHubs {
    fn resource_type(&self) -> &'static str {
        HUB_TYPE
    }

    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        let given = |member: &str| request.body.pointer(&format!("/properties/{member}")).filter(|v| !v.is_null()).cloned();
        let tier = tier(ctx, request.id);
        let mut properties = match request.existing {
            Some(existing) => existing.properties.clone(),
            None => json!({
                "partitionCount": 4,
                "messageRetentionInDays": 1,
                "status": "Active",
                "createdAt": iso_timestamp(),
            }),
        };
        if let Some(count) = given("partitionCount") {
            let count = count.as_u64().ok_or_else(|| invalid("The partitionCount must be a number."))?;
            if request.existing.is_some() && Some(count) != properties["partitionCount"].as_u64() {
                return Err(invalid("The partition count of an event hub cannot be changed."));
            }
            let max = if tier.eq_ignore_ascii_case("Premium") { 100 } else { 32 };
            if !(1..=max).contains(&count) {
                return Err(invalid(format!("The partitionCount must be between 1 and {max} in the '{tier}' tier.")));
            }
            properties["partitionCount"] = json!(count);
        }
        if let Some(days) = given("messageRetentionInDays") {
            let days = days.as_u64().ok_or_else(|| invalid("The messageRetentionInDays must be a number."))?;
            let max = match tier.to_ascii_lowercase().as_str() {
                "basic" => 1,
                "premium" => 90,
                _ => 7,
            };
            if !(1..=max).contains(&days) {
                return Err(invalid(format!("The messageRetentionInDays must be between 1 and {max} in the '{tier}' tier.")));
            }
            properties["messageRetentionInDays"] = json!(days);
        }
        for member in ["status", "captureDescription", "retentionDescription", "userMetadata"] {
            if let Some(value) = given(member) {
                properties[member] = value;
            }
        }
        let count = properties["partitionCount"].as_u64().unwrap_or(4);
        properties["partitionIds"] = json!((0..count).map(|i| i.to_string()).collect::<Vec<_>>());
        properties["updatedAt"] = json!(iso_timestamp());
        Ok(properties)
    }

    fn patch(&self, ctx: &ArmContext<'_>, existing: &Resource, body: &Value) -> Result<Value, ArmError> {
        self.put(ctx, &PutRequest { id: &existing.id, name: &existing.name, location: None, existing: Some(existing), body })
    }

    fn created(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        // Every event hub has the default consumer group.
        ctx.engine.state::<ArmState>().lock().insert(Resource {
            id: format!("{}/consumergroups/$Default", resource.id),
            name: "$Default".into(),
            resource_type: GROUP_TYPE.into(),
            location: None,
            tags: None,
            kind: None,
            sku: None,
            identity: None,
            properties: json!({"createdAt": iso_timestamp(), "updatedAt": iso_timestamp()}),
        });
    }

    fn deleted(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        let namespace = resource.id.split('/').skip_while(|p| !p.eq_ignore_ascii_case("namespaces")).nth(1).unwrap_or_default();
        service_servicebus::broker::eventhubs::forget(ctx.engine, &format!("{namespace}/{}", resource.name));
    }
}

pub(crate) struct ConsumerGroups;

impl ResourceTypeHandler for ConsumerGroups {
    fn resource_type(&self) -> &'static str {
        GROUP_TYPE
    }

    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        if request.existing.is_none() && tier(ctx, request.id).eq_ignore_ascii_case("Basic") && request.name != "$Default" {
            return Err(invalid("A Basic namespace has only the $Default consumer group."));
        }
        let mut properties = match request.existing {
            Some(existing) => existing.properties.clone(),
            None => json!({"createdAt": iso_timestamp()}),
        };
        if let Some(metadata) = request.body.pointer("/properties/userMetadata").filter(|v| !v.is_null()) {
            properties["userMetadata"] = metadata.clone();
        }
        properties["updatedAt"] = json!(iso_timestamp());
        Ok(properties)
    }

    fn patch(&self, ctx: &ArmContext<'_>, existing: &Resource, body: &Value) -> Result<Value, ArmError> {
        self.put(ctx, &PutRequest { id: &existing.id, name: &existing.name, location: None, existing: Some(existing), body })
    }

    fn before_delete(&self, _ctx: &ArmContext<'_>, resource: &Resource) -> Result<(), ArmError> {
        if resource.name == "$Default" {
            return Err(invalid("The $Default consumer group cannot be deleted."));
        }
        Ok(())
    }
}
