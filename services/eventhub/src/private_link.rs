//! Private link resources: the sub-resource a private endpoint can target on a
//! namespace. Event Hubs namespaces offer one, `namespace`, as Service Bus
//! namespaces do (`service_servicebus` `geo.rs`).

use std::sync::Arc;

use emulator_core::arm::{ArmContext, ArmRegistry, ArmState, OperationRequest};
use emulator_core::ArmError;
use http::StatusCode;
use serde_json::{json, Value};

pub(crate) fn register(registry: &mut ArmRegistry) {
    registry.register_operation("Microsoft.EventHub/PrivateLinkResources_Get", Arc::new(private_link_resources));
}

fn private_link_resources(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Result<(StatusCode, Value), ArmError> {
    let name = request.param("namespaceName");
    let id = format!(
        "/subscriptions/{}/resourceGroups/{}/providers/Microsoft.EventHub/namespaces/{name}",
        ctx.subscription_id, ctx.resource_group
    );
    let namespace = ctx.engine.state::<ArmState>().lock().resource(&id).cloned();
    let namespace = namespace.ok_or_else(|| {
        ArmError::not_found(
            "ResourceNotFound",
            format!(
                "The Resource 'Microsoft.EventHub/namespaces/{name}' under resource group '{}' was not found. For more details please go to https://aka.ms/ARMResourceNotFoundFix",
                ctx.resource_group
            ),
        )
    })?;
    // The group and its private DNS zone are those of the specification's
    // example and Azure's private endpoint DNS documentation: Event Hubs shares
    // the Service Bus zone. The zone is a name for a private DNS zone, not an
    // address a client connects to.
    Ok((
        StatusCode::OK,
        json!({"value": [{
            "id": format!("{}/privateLinkResources/namespace", namespace.id),
            "name": "namespace",
            "type": "Microsoft.EventHub/namespaces/privateLinkResources",
            "properties": {
                "groupId": "namespace",
                "requiredMembers": ["namespace"],
                // Unverified against Azure: the specification's example gives
                // `privatelink.EventHub.windows.net`, while Azure's private
                // endpoint DNS documentation puts Event Hubs in the Service
                // Bus zone, which is what this returns.
                "requiredZoneNames": ["privatelink.servicebus.windows.net"],
            },
        }]}),
    ))
}
