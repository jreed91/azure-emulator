//! Geo-disaster-recovery aliases, the legacy network filter rules, and private link
//! resources.
//!
//! An alias pairs a primary namespace with a secondary one. The pairing exists as
//! one configuration on each namespace, under the same alias name: `Primary` on
//! one side, `Secondary` on the other. Breaking the pairing or failing over
//! leaves a single `PrimaryNotReplicating` configuration on the surviving side.
//!
//! Event Hubs namespaces have the same aliases ([`EVENT_HUBS_GEO`]); the
//! `service-eventhub` crate mounts them with [`register_aliases`].

use std::sync::Arc;

use emulator_core::arm::{
    ArmContext, ArmRegistry, ArmState, OperationRequest, PutRequest, Resource, ResourceTypeHandler,
};
use emulator_core::ArmError;
use http::StatusCode;
use serde_json::{json, Value};

use crate::namespaces::{id_part, invalid, is_valid_namespace_name, rule_keys};
use crate::ServiceBusState;

/// One provider's geo-disaster-recovery aliases: the types and operations they
/// are served under, and the tiers that can be paired.
pub struct GeoFlavor {
    pub namespace: &'static str,
    pub alias: &'static str,
    /// The tiers a pairing may join; both namespaces must be of the same one.
    pub tiers: &'static [&'static str],
    /// `operationId`s of `CheckNameAvailability`, `ListAuthorizationRules`,
    /// `GetAuthorizationRule` and `ListKeys`.
    pub operations: [&'static str; 4],
}

pub const SERVICE_BUS_GEO: GeoFlavor = GeoFlavor {
    namespace: "Microsoft.ServiceBus/namespaces",
    alias: "Microsoft.ServiceBus/namespaces/disasterRecoveryConfigs",
    tiers: &["Premium"],
    operations: [
        "Microsoft.ServiceBus/DisasterRecoveryConfigs_CheckNameAvailability",
        "Microsoft.ServiceBus/DisasterRecoveryConfigs_ListAuthorizationRules",
        "Microsoft.ServiceBus/DisasterRecoveryConfigs_GetAuthorizationRule",
        "Microsoft.ServiceBus/DisasterRecoveryConfigs_ListKeys",
    ],
};

/// Event Hubs pairs Standard with Standard and Premium with Premium (Dedicated
/// clusters are not modelled).
pub const EVENT_HUBS_GEO: GeoFlavor = GeoFlavor {
    namespace: "Microsoft.EventHub/namespaces",
    alias: "Microsoft.EventHub/namespaces/disasterRecoveryConfigs",
    tiers: &["Standard", "Premium"],
    operations: [
        "Microsoft.EventHub/DisasterRecoveryConfigs_CheckNameAvailability",
        "Microsoft.EventHub/DisasterRecoveryConfigs_ListAuthorizationRules",
        "Microsoft.EventHub/DisasterRecoveryConfigs_GetAuthorizationRule",
        "Microsoft.EventHub/DisasterRecoveryConfigs_ListKeys",
    ],
};

/// Mounts one provider's aliases, their authorization rules and keys.
pub fn register_aliases(registry: &mut ArmRegistry, flavor: &'static GeoFlavor) {
    registry.register_type(Aliases(flavor));
    let [check, list, get, keys] = flavor.operations;
    registry.register_operation(check, Arc::new(move |ctx, request| check_alias_name(flavor, ctx, request)));
    registry.register_operation(list, Arc::new(move |ctx, request| list_alias_rules(flavor, ctx, request)));
    registry.register_operation(get, Arc::new(move |ctx, request| get_alias_rule(flavor, ctx, request)));
    registry.register_operation(keys, Arc::new(move |ctx, request| alias_rule_keys(flavor, ctx, request)));
}

pub(crate) fn register(registry: &mut ArmRegistry) {
    register_aliases(registry, &SERVICE_BUS_GEO);
    registry.register_type(FilterRules {
        resource_type: "Microsoft.ServiceBus/namespaces/ipfilterrules",
        required: "ipMask",
    });
    registry.register_type(FilterRules {
        resource_type: "Microsoft.ServiceBus/namespaces/virtualnetworkrules",
        required: "virtualNetworkSubnetId",
    });
    registry.register_operation("Microsoft.ServiceBus/PrivateLinkResources_Get", Arc::new(private_link_resources));
}

/// The id of the namespace a child resource id belongs to.
fn namespace_id(id: &str) -> String {
    let lower = id.to_ascii_lowercase();
    let end = lower
        .find("/namespaces/")
        .map(|at| at + "/namespaces/".len())
        .map(|start| lower[start..].find('/').map(|slash| start + slash).unwrap_or(id.len()))
        .unwrap_or(id.len());
    id[..end].to_string()
}

fn tier(namespace: &Resource) -> &str {
    namespace.sku.as_ref().and_then(|sku| sku.get("name")).and_then(Value::as_str).unwrap_or("")
}

struct Aliases(&'static GeoFlavor);

impl Aliases {
    /// The other namespace's configuration of the same alias.
    fn mirror_id(resource: &Resource) -> Option<String> {
        let partner = resource.properties.get("partnerNamespace").and_then(Value::as_str).filter(|p| !p.is_empty())?;
        Some(format!("{partner}/disasterRecoveryConfigs/{}", resource.name))
    }

    /// Ends the pairing, leaving `resource`'s namespace as the only holder of the
    /// alias. `expected_role` is the role the namespace must hold to do this.
    fn unpair(ctx: &ArmContext<'_>, resource: &Resource, expected_role: &str, verb: &str) -> Result<Value, ArmError> {
        if resource.properties["role"] != expected_role {
            return Err(invalid(format!(
                "The alias '{}' can only {verb} from the {} namespace of a pairing.",
                resource.name,
                expected_role.to_ascii_lowercase()
            )));
        }
        let state = ctx.engine.state::<ArmState>();
        let mut state = state.lock();
        if let Some(mirror) = Self::mirror_id(resource) {
            state.remove(&mirror);
        }
        if let Some(own) = state.resource_mut(&resource.id) {
            own.properties["role"] = json!("PrimaryNotReplicating");
            own.properties["partnerNamespace"] = json!("");
        }
        Ok(Value::Null)
    }
}

impl ResourceTypeHandler for Aliases {
    fn resource_type(&self) -> &'static str {
        self.0.alias
    }

    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        let alternate_name = request.body.pointer("/properties/alternateName").filter(|v| !v.is_null());
        if let Some(existing) = request.existing {
            // The pairing itself is fixed; only the alternate name can change.
            let mut properties = existing.properties.clone();
            if let Some(name) = alternate_name {
                properties["alternateName"] = name.clone();
            }
            return Ok(properties);
        }

        if !is_valid_namespace_name(request.name) {
            return Err(invalid("The alias name is invalid. It follows the same rules as a namespace name."));
        }
        let partner_id = request
            .body
            .pointer("/properties/partnerNamespace")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| invalid("The request content was invalid: the required property 'properties.partnerNamespace' is missing."))?;
        let own_id = namespace_id(request.id);
        let (own, partner) = {
            let state = ctx.engine.state::<ArmState>();
            let state = state.lock();
            (state.resource(&own_id).cloned(), state.resource(partner_id).cloned())
        };
        let own = own.ok_or_else(|| invalid("The namespace of the alias does not exist."))?;
        let partner = partner
            .filter(|p| p.resource_type.eq_ignore_ascii_case(self.0.namespace))
            .ok_or_else(|| invalid(format!("The partner namespace '{partner_id}' does not exist.")))?;
        if partner.id.eq_ignore_ascii_case(&own.id) {
            return Err(invalid("A namespace cannot be paired with itself."));
        }
        // Unverified against Azure: the wording of this refusal, and for Event
        // Hubs that a Standard namespace cannot pair with a Premium one.
        if !self.0.tiers.contains(&tier(&own)) || tier(&own) != tier(&partner) {
            return Err(invalid(format!(
                "Geo-disaster recovery requires both namespaces to be of the same tier, one of: {}.",
                self.0.tiers.join(", ")
            )));
        }
        // An alias is a DNS name in the same space as namespace names.
        let state = ctx.engine.state::<ServiceBusState>();
        if !state.lock().namespaces.insert(request.name.to_ascii_lowercase()) {
            return Err(ArmError::conflict(
                "Conflict",
                format!("The alias name '{}' is already in use.", request.name),
            ));
        }
        let mut properties = json!({
            "provisioningState": "Succeeded",
            "partnerNamespace": partner.id,
            "role": "Primary",
            "pendingReplicationOperationsCount": 0,
        });
        if let Some(name) = alternate_name {
            properties["alternateName"] = name.clone();
        }
        Ok(properties)
    }

    fn patch(&self, ctx: &ArmContext<'_>, existing: &Resource, body: &Value) -> Result<Value, ArmError> {
        self.put(
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

    fn created(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        // The partner sees the same alias from the other side.
        let Some(mirror_id) = Self::mirror_id(resource) else {
            return;
        };
        let mut properties = resource.properties.clone();
        properties["role"] = json!("Secondary");
        properties["partnerNamespace"] = json!(namespace_id(&resource.id));
        ctx.engine.state::<ArmState>().lock().insert(Resource {
            id: mirror_id,
            properties,
            ..resource.clone()
        });
    }

    fn actions(&self) -> &'static [&'static str] {
        &["breakPairing", "failover"]
    }

    fn action(&self, ctx: &ArmContext<'_>, resource: &Resource, action: &str, _body: &Value) -> Result<Value, ArmError> {
        match action.to_ascii_lowercase().as_str() {
            "breakpairing" => Self::unpair(ctx, resource, "Primary", "be broken"),
            "failover" => Self::unpair(ctx, resource, "Secondary", "fail over"),
            other => Err(ArmError::not_implemented(format_args!("the '{other}' action on {}", self.0.alias))),
        }
    }

    fn deleted(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        let still_held = {
            let state = ctx.engine.state::<ArmState>();
            let mut state = state.lock();
            match Self::mirror_id(resource) {
                // Deleting the primary's configuration dissolves the pairing.
                Some(mirror) if resource.properties["role"] == "Primary" => {
                    state.remove(&mirror);
                    false
                }
                Some(mirror) => state.resource(&mirror).is_some(),
                None => false,
            }
        };
        if !still_held {
            ctx.engine
                .state::<ServiceBusState>()
                .lock()
                .namespaces
                .remove(&resource.name.to_ascii_lowercase());
        }
    }
}

type Reply = Result<(StatusCode, Value), ArmError>;

fn require_namespace(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Result<Resource, ArmError> {
    require_namespace_of(SERVICE_BUS_GEO.namespace, ctx, request)
}

/// The namespace of type `namespace_type` that the request's `namespaceName` names.
fn require_namespace_of(namespace_type: &str, ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Result<Resource, ArmError> {
    let name = request.param("namespaceName");
    let id = format!(
        "/subscriptions/{}/resourceGroups/{}/providers/{namespace_type}/{name}",
        ctx.subscription_id, ctx.resource_group
    );
    let state = ctx.engine.state::<ArmState>();
    let state = state.lock();
    state.resource(&id).cloned().ok_or_else(|| {
        ArmError::not_found(
            "ResourceNotFound",
            format!(
                "The Resource '{namespace_type}/{name}' under resource group '{}' was not found. For more details please go to https://aka.ms/ARMResourceNotFoundFix",
                ctx.resource_group
            ),
        )
    })
}

fn check_alias_name(flavor: &GeoFlavor, ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    require_namespace_of(flavor.namespace, ctx, request)?;
    let name = request
        .body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("The request content was invalid: the required property 'name' is missing."))?;
    let body = if !is_valid_namespace_name(name) {
        json!({"nameAvailable": false, "reason": "InvalidName", "message": "The specified name is invalid."})
    } else if ctx.engine.state::<ServiceBusState>().lock().namespaces.contains(&name.to_ascii_lowercase()) {
        json!({"nameAvailable": false, "reason": "NameInUse", "message": "The specified name is already in use."})
    } else {
        json!({"nameAvailable": true, "reason": "None", "message": ""})
    };
    Ok((StatusCode::OK, body))
}

/// The namespace's own authorization rules, presented under the alias.
fn alias_rules(flavor: &GeoFlavor, ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Result<Vec<(Resource, Value)>, ArmError> {
    let namespace = require_namespace_of(flavor.namespace, ctx, request)?;
    let alias = request.param("alias");
    let state = ctx.engine.state::<ArmState>();
    let state = state.lock();
    let config = state
        .resource(&format!("{}/disasterRecoveryConfigs/{alias}", namespace.id))
        .ok_or_else(|| ArmError::not_found("ResourceNotFound", format!("The alias '{alias}' was not found.")))?;
    Ok(state
        .children(&format!("{}/AuthorizationRules", namespace.id))
        .map(|rule| {
            let view = json!({
                "id": format!("{}/authorizationRules/{}", config.id, rule.name),
                "name": rule.name,
                "type": format!("{}/authorizationRules", flavor.alias),
                "properties": rule.properties,
            });
            (rule.clone(), view)
        })
        .collect())
}

fn find_alias_rule(flavor: &GeoFlavor, ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Result<(Resource, Value), ArmError> {
    let wanted = request.param("authorizationRuleName");
    alias_rules(flavor, ctx, request)?
        .into_iter()
        .find(|(rule, _)| rule.name.eq_ignore_ascii_case(wanted))
        .ok_or_else(|| ArmError::not_found("ResourceNotFound", format!("The authorization rule '{wanted}' was not found.")))
}

fn list_alias_rules(flavor: &GeoFlavor, ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let views: Vec<Value> = alias_rules(flavor, ctx, request)?.into_iter().map(|(_, view)| view).collect();
    Ok((StatusCode::OK, json!({"value": views})))
}

fn get_alias_rule(flavor: &GeoFlavor, ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    Ok((StatusCode::OK, find_alias_rule(flavor, ctx, request)?.1))
}

fn alias_rule_keys(flavor: &GeoFlavor, ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let (rule, _) = find_alias_rule(flavor, ctx, request)?;
    let keys = rule_keys(ctx, &rule.id);
    let config = ctx.engine.config();
    let endpoint = |host: &str| config.servicebus_amqp_endpoint(&host.to_ascii_lowercase());
    let namespace = id_part(&rule.id, "namespaces").unwrap_or("");
    let connection = |host: &str, key: &str| {
        format!("Endpoint={};SharedAccessKeyName={};SharedAccessKey={key};UseDevelopmentEmulator=true", endpoint(host), rule.name)
    };
    // The alias connection strings keep working across a failover.
    Ok((
        StatusCode::OK,
        json!({
            "aliasPrimaryConnectionString": connection(request.param("alias"), &keys[0]),
            "aliasSecondaryConnectionString": connection(request.param("alias"), &keys[1]),
            "primaryConnectionString": connection(namespace, &keys[0]),
            "secondaryConnectionString": connection(namespace, &keys[1]),
            "primaryKey": keys[0],
            "secondaryKey": keys[1],
            "keyName": rule.name,
        }),
    ))
}

fn private_link_resources(ctx: &ArmContext<'_>, request: &OperationRequest<'_>) -> Reply {
    let namespace = require_namespace(ctx, request)?;
    Ok((
        StatusCode::OK,
        json!({"value": [{
            "id": format!("{}/privateLinkResources/namespace", namespace.id),
            "name": "namespace",
            "type": "Microsoft.ServiceBus/namespaces/privateLinkResources",
            "properties": {
                "groupId": "namespace",
                "requiredMembers": ["namespace"],
                "requiredZoneNames": ["privatelink.servicebus.windows.net"],
            },
        }]}),
    ))
}

/// The legacy per-namespace IP filter and virtual network rules: named records
/// with one required setting each.
struct FilterRules {
    resource_type: &'static str,
    required: &'static str,
}

impl ResourceTypeHandler for FilterRules {
    fn resource_type(&self) -> &'static str {
        self.resource_type
    }

    fn put(&self, _ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        let given = |member: &str| request.body.pointer(&format!("/properties/{member}")).filter(|v| !v.is_null());
        let setting = given(self.required)
            .filter(|value| value.as_str().is_some_and(|text| !text.is_empty()))
            .ok_or_else(|| {
                invalid(format!(
                    "The request content was invalid: the required property 'properties.{}' is missing.",
                    self.required
                ))
            })?;
        let mut properties = json!({});
        properties[self.required] = setting.clone();
        if self.required == "ipMask" {
            let action = given("action").cloned().unwrap_or_else(|| json!("Accept"));
            if !["Accept", "Reject"].contains(&action.as_str().unwrap_or("")) {
                return Err(invalid("The value of 'properties.action' must be Accept or Reject."));
            }
            properties["action"] = action;
            properties["filterName"] = given("filterName").cloned().unwrap_or_else(|| json!(request.name));
        }
        Ok(properties)
    }

    fn patch(&self, ctx: &ArmContext<'_>, existing: &Resource, body: &Value) -> Result<Value, ArmError> {
        // These rules have no partial update: a PATCH must carry the whole rule.
        self.put(
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
}
