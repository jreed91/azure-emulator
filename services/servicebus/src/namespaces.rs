//! Namespaces and authorization rules.

use base64::Engine as _;
use emulator_core::arm::{ArmContext, ArmState, PutRequest, Resource, ResourceTypeHandler};
use emulator_core::http_util::iso_timestamp;
use emulator_core::ArmError;
use rand::RngCore;
use serde_json::{json, Value};

use crate::ServiceBusState;

const NAMESPACE_RULE_TYPE: &str = "Microsoft.ServiceBus/namespaces/AuthorizationRules";

/// Service Bus and Event Hubs namespaces work alike: the same names (unique
/// across both, as on Azure), rules and keys. A flavor names one provider's
/// types.
pub struct Flavor {
    pub namespace: &'static str,
    pub rule: &'static str,
    pub network_rule_set: &'static str,
    /// Event Hubs namespaces report throughput units and Kafka.
    pub event_hubs: bool,
}

pub const SERVICE_BUS: Flavor = Flavor {
    namespace: "Microsoft.ServiceBus/namespaces",
    rule: NAMESPACE_RULE_TYPE,
    network_rule_set: "Microsoft.ServiceBus/namespaces/networkRuleSets",
    event_hubs: false,
};

pub const EVENT_HUBS: Flavor = Flavor {
    namespace: "Microsoft.EventHub/namespaces",
    rule: "Microsoft.EventHub/namespaces/authorizationRules",
    network_rule_set: "Microsoft.EventHub/namespaces/networkRuleSets",
    event_hubs: true,
};

/// The three levels an authorization rule can be defined at.
pub(crate) const AUTHORIZATION_RULE_TYPES: [&str; 3] = [
    NAMESPACE_RULE_TYPE,
    "Microsoft.ServiceBus/namespaces/queues/authorizationRules",
    "Microsoft.ServiceBus/namespaces/topics/authorizationRules",
];

/// Properties a caller may set on a namespace; everything else is service-owned.
const SETTABLE: [&str; 9] = [
    "zoneRedundant",
    "disableLocalAuth",
    "minimumTlsVersion",
    "publicNetworkAccess",
    "encryption",
    "alternateName",
    "premiumMessagingPartitions",
    "geoDataReplication",
    "platformCapabilities",
];

pub(crate) fn invalid(message: impl Into<String>) -> ArmError {
    ArmError::bad_request("BadRequest", message)
}

pub(crate) fn is_valid_namespace_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (6..=50).contains(&bytes.len())
        && bytes.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-')
        && bytes[0].is_ascii_alphabetic()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
}

const NAME_RULE: &str = "The specified service namespace is invalid. The namespace name must be 6 to 50 characters long, contain only letters, numbers and hyphens, start with a letter and end with a letter or number.";

fn apply_event_hubs_settable(properties: &mut Value, body: &Value) {
    for member in ["isAutoInflateEnabled", "maximumThroughputUnits", "kafkaEnabled"] {
        if let Some(value) = body.pointer(&format!("/properties/{member}")).filter(|v| !v.is_null()) {
            properties[member] = value.clone();
        }
    }
}

fn apply_settable(properties: &mut Value, body: &Value) {
    for member in SETTABLE {
        if let Some(value) = body.pointer(&format!("/properties/{member}")).filter(|v| !v.is_null()) {
            properties[member] = value.clone();
        }
    }
    properties["updatedAt"] = json!(iso_timestamp());
}

/// A namespace's network rule set: a singleton named `default` that exists from
/// the moment the namespace does.
pub struct NetworkRuleSets(pub &'static Flavor);

impl ResourceTypeHandler for NetworkRuleSets {
    fn resource_type(&self) -> &'static str {
        self.0.network_rule_set
    }

    fn put(&self, _ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        let existing = request.existing.ok_or_else(|| {
            ArmError::not_found(
                "ResourceNotFound",
                format!("The network rule set '{}' does not exist; the only name is 'default'.", request.name),
            )
        })?;
        let mut properties = existing.properties.clone();
        let given = request.body.get("properties").and_then(Value::as_object);
        for (member, allowed) in [
            ("defaultAction", &["Allow", "Deny"][..]),
            ("publicNetworkAccess", &["Enabled", "Disabled", "SecuredByPerimeter"][..]),
        ] {
            if let Some(value) = given.and_then(|g| g.get(member)).filter(|v| !v.is_null()) {
                if !value.as_str().is_some_and(|v| allowed.contains(&v)) {
                    return Err(invalid(format!(
                        "The value of 'properties.{member}' must be one of: {}.",
                        allowed.join(", ")
                    )));
                }
            }
        }
        for member in ["publicNetworkAccess", "defaultAction", "virtualNetworkRules", "ipRules", "trustedServiceAccessEnabled"] {
            if let Some(value) = given.and_then(|g| g.get(member)).filter(|v| !v.is_null()) {
                properties[member] = value.clone();
            }
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
}

pub struct Namespaces(pub &'static Flavor);

impl ResourceTypeHandler for Namespaces {
    fn resource_type(&self) -> &'static str {
        self.0.namespace
    }

    fn put(&self, ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        let name = request.name;
        if !is_valid_namespace_name(name) {
            return Err(invalid(NAME_RULE));
        }
        if let Some(sku) = request.body.get("sku").filter(|s| !s.is_null()) {
            let tier = sku.get("name").and_then(Value::as_str).unwrap_or("");
            if !["Basic", "Standard", "Premium"].contains(&tier) {
                return Err(invalid(format!(
                    "The SKU name '{tier}' is invalid. Valid values are Basic, Standard and Premium."
                )));
            }
        }
        let mut properties = match request.existing {
            Some(existing) => existing.properties.clone(),
            None => {
                let state = ctx.engine.state::<ServiceBusState>();
                if !state.lock().namespaces.insert(name.to_ascii_lowercase()) {
                    return Err(ArmError::conflict(
                        "Conflict",
                        format!("The specified service namespace '{name}' is already in use. Namespace names are unique across Azure."),
                    ));
                }
                let mut properties = json!({
                    "provisioningState": "Succeeded",
                    "status": "Active",
                    "createdAt": iso_timestamp(),
                    "serviceBusEndpoint": ctx.engine.config().servicebus_endpoint(&name.to_ascii_lowercase()),
                    "metricId": format!("{}:{}", ctx.subscription_id, name.to_ascii_lowercase()),
                    "zoneRedundant": false,
                    "disableLocalAuth": false,
                    "minimumTlsVersion": "1.2",
                    "publicNetworkAccess": "Enabled",
                });
                if self.0.event_hubs {
                    let tier = request.body.pointer("/sku/name").and_then(Value::as_str).unwrap_or("Standard");
                    properties["kafkaEnabled"] = json!(tier != "Basic");
                    properties["isAutoInflateEnabled"] = json!(false);
                    properties["maximumThroughputUnits"] = json!(0);
                }
                properties
            }
        };
        apply_settable(&mut properties, request.body);
        if self.0.event_hubs {
            apply_event_hubs_settable(&mut properties, request.body);
        }
        Ok(properties)
    }

    fn patch(&self, _ctx: &ArmContext<'_>, existing: &Resource, body: &Value) -> Result<Value, ArmError> {
        let mut properties = existing.properties.clone();
        apply_settable(&mut properties, body);
        if self.0.event_hubs {
            apply_event_hubs_settable(&mut properties, body);
        }
        Ok(properties)
    }

    fn prepare(&self, _ctx: &ArmContext<'_>, resource: &mut Resource) {
        // A namespace created without a SKU is Standard; the tier mirrors the name.
        let mut sku = resource.sku.take().unwrap_or_else(|| json!({"name": "Standard"}));
        if sku.get("tier").is_none() {
            sku["tier"] = sku["name"].clone();
        }
        // Premium has messaging units; Event Hubs has throughput units too.
        if (sku["name"] == "Premium" || self.0.event_hubs) && sku.get("capacity").is_none() {
            sku["capacity"] = json!(1);
        }
        resource.sku = Some(sku);
    }

    fn created(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        // Every namespace is born with a full-rights rule.
        ctx.engine.state::<ArmState>().lock().insert(Resource {
            id: format!("{}/AuthorizationRules/RootManageSharedAccessKey", resource.id),
            name: "RootManageSharedAccessKey".into(),
            resource_type: self.0.rule.into(),
            location: None,
            tags: None,
            kind: None,
            sku: None,
            identity: None,
            properties: json!({"rights": ["Listen", "Manage", "Send"]}),
        });
        // ... and with an open network rule set.
        ctx.engine.state::<ArmState>().lock().insert(Resource {
            id: format!("{}/networkRuleSets/default", resource.id),
            name: "default".into(),
            resource_type: self.0.network_rule_set.into(),
            location: None,
            tags: None,
            kind: None,
            sku: None,
            identity: None,
            properties: json!({
                "publicNetworkAccess": "Enabled",
                "defaultAction": "Allow",
                "virtualNetworkRules": [],
                "ipRules": [],
                "trustedServiceAccessEnabled": false,
            }),
        });
    }

    fn deleted(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        ctx.engine
            .state::<ServiceBusState>()
            .lock()
            .namespaces
            .remove(&resource.name.to_ascii_lowercase());
        crate::entities::forget_messages(ctx, resource);
        if self.0.event_hubs {
            crate::broker::eventhubs::forget(ctx.engine, &resource.name);
        }
    }
}

pub fn check_name_availability(ctx: &ArmContext<'_>, body: &Value) -> Result<Value, ArmError> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("The request content was invalid: the required property 'name' is missing."))?;
    if !is_valid_namespace_name(name) {
        return Ok(json!({"nameAvailable": false, "reason": "InvalidName", "message": NAME_RULE}));
    }
    let taken = ctx
        .engine
        .state::<ServiceBusState>()
        .lock()
        .namespaces
        .contains(&name.to_ascii_lowercase());
    Ok(if taken {
        json!({
            "nameAvailable": false,
            "reason": "NameInUse",
            "message": "The specified service namespace is already in use.",
        })
    } else {
        json!({"nameAvailable": true, "reason": "None", "message": ""})
    })
}

/// A new 256-bit shared access key, base64 encoded.
pub(crate) fn new_key() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The name that follows `segment` in a resource id, e.g. the queue name.
pub(crate) fn id_part<'a>(id: &'a str, segment: &str) -> Option<&'a str> {
    let mut parts = id.split('/');
    parts.find(|part| part.eq_ignore_ascii_case(segment))?;
    parts.next()
}

/// The primary and secondary key of the authorization rule `rule_id`, minted the
/// first time anything asks for them.
pub(crate) fn rule_keys(ctx: &ArmContext<'_>, rule_id: &str) -> [String; 2] {
    let state = ctx.engine.state::<ServiceBusState>();
    let mut state = state.lock();
    state
        .keys
        .entry(rule_id.to_ascii_lowercase())
        .or_insert_with(|| [new_key(), new_key()])
        .clone()
}

/// Authorization rules at one of the three levels; the level is the type name.
pub struct AuthorizationRules(pub &'static str);

impl AuthorizationRules {
    fn keys_json(&self, ctx: &ArmContext<'_>, resource: &Resource, keys: &[String; 2]) -> Value {
        let namespace = id_part(&resource.id, "namespaces").unwrap_or("").to_ascii_lowercase();
        let endpoint = ctx.engine.config().servicebus_amqp_endpoint(&namespace);
        let entity = id_part(&resource.id, "queues")
            .or_else(|| id_part(&resource.id, "eventhubs"))
            .or_else(|| id_part(&resource.id, "topics"))
            .map(|entity| format!(";EntityPath={entity}"))
            .unwrap_or_default();
        // The SDKs reach the emulator's broker in their development-emulator
        // mode: plain AMQP on the endpoint's port.
        let connection = |key: &str| {
            format!(
                "Endpoint={endpoint};SharedAccessKeyName={};SharedAccessKey={key}{entity};UseDevelopmentEmulator=true",
                resource.name
            )
        };
        json!({
            "primaryConnectionString": connection(&keys[0]),
            "secondaryConnectionString": connection(&keys[1]),
            "primaryKey": keys[0],
            "secondaryKey": keys[1],
            "keyName": resource.name,
        })
    }
}

impl ResourceTypeHandler for AuthorizationRules {
    fn resource_type(&self) -> &'static str {
        self.0
    }

    fn put(&self, _ctx: &ArmContext<'_>, request: &PutRequest<'_>) -> Result<Value, ArmError> {
        let rights: Vec<&str> = request
            .body
            .pointer("/properties/rights")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("The request content was invalid: the required property 'properties.rights' is missing."))?
            .iter()
            .filter_map(Value::as_str)
            .collect();
        if rights.is_empty() || rights.iter().any(|r| !["Listen", "Send", "Manage"].contains(r)) {
            return Err(invalid("The rights of an authorization rule must be one or more of Listen, Send and Manage."));
        }
        if rights.contains(&"Manage") && !(rights.contains(&"Listen") && rights.contains(&"Send")) {
            return Err(invalid("Manage permission should also include Send and Listen."));
        }
        Ok(json!({"rights": rights}))
    }

    fn patch(&self, _ctx: &ArmContext<'_>, existing: &Resource, _body: &Value) -> Result<Value, ArmError> {
        Ok(existing.properties.clone())
    }

    fn actions(&self) -> &'static [&'static str] {
        &["listKeys", "regenerateKeys"]
    }

    fn action(
        &self,
        ctx: &ArmContext<'_>,
        resource: &Resource,
        action: &str,
        body: &Value,
    ) -> Result<Value, ArmError> {
        let state = ctx.engine.state::<ServiceBusState>();
        let mut state = state.lock();
        // Keys are minted the first time anything asks for them.
        let keys = state
            .keys
            .entry(resource.id.to_ascii_lowercase())
            .or_insert_with(|| [new_key(), new_key()]);
        match action.to_ascii_lowercase().as_str() {
            "listkeys" => Ok(self.keys_json(ctx, resource, keys)),
            "regeneratekeys" => {
                let index = match body.get("keyType").and_then(Value::as_str) {
                    Some("PrimaryKey") => 0,
                    Some("SecondaryKey") => 1,
                    _ => return Err(invalid("The keyType must be PrimaryKey or SecondaryKey.")),
                };
                // A caller may supply the key value instead of having one generated.
                keys[index] = match body.get("key").and_then(Value::as_str).filter(|k| !k.is_empty()) {
                    Some(supplied) => supplied.to_string(),
                    None => new_key(),
                };
                Ok(self.keys_json(ctx, resource, keys))
            }
            other => Err(ArmError::not_implemented(format_args!("the '{other}' action on {}", self.0))),
        }
    }

    fn deleted(&self, ctx: &ArmContext<'_>, resource: &Resource) {
        ctx.engine
            .state::<ServiceBusState>()
            .lock()
            .keys
            .remove(&resource.id.to_ascii_lowercase());
    }
}
