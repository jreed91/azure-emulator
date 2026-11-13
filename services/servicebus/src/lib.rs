//! `Microsoft.ServiceBus` resource provider.
//!
//! Every resource type in the provider's API specification is mounted from
//! [`scaffold::PROVIDER`] and answers `501` until it is implemented. To implement a
//! type, write a `ResourceTypeHandler` for it and register it here, before the
//! scaffold. `CONTRACT.md` describes what the specification says about each type.
//!
//! Implemented: namespaces, queues, topics, subscriptions, rules, and authorization
//! rules with their keys at namespace, queue and topic level; and [`broker`], the
//! AMQP 1.0 broker that serves messaging for Service Bus and Event Hubs namespaces
//! alike (Event Hubs' resource provider is the `service-eventhub` crate).
//!
//! Also implemented: geo-disaster-recovery aliases (pairing, break and failover as
//! control-plane state; nothing is replicated because there is no data plane), the
//! legacy IP filter and virtual network rules, and private link resources.
//!
//! Not checked against Azure: the code and wording of validation refusals here, and
//! the tier restrictions (topics on a Basic namespace are accepted).

pub mod broker;
mod entities;
mod geo;
mod migration;
pub mod namespaces;
pub mod scaffold;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use emulator_core::arm::ArmRegistry;

/// Service Bus control-plane state beyond the resources themselves.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct ServiceBusState {
    /// Lowercased namespace names in use; they are global DNS labels.
    pub(crate) namespaces: BTreeSet<String>,
    /// Lowercased authorization rule id → its primary and secondary key.
    pub(crate) keys: HashMap<String, [String; 2]>,
}

/// Mounts the provider on Resource Manager.
pub fn register(registry: &mut ArmRegistry) {
    registry.register_type(namespaces::Namespaces(&namespaces::SERVICE_BUS));
    registry.register_type(namespaces::NetworkRuleSets(&namespaces::SERVICE_BUS));
    for kind in namespaces::AUTHORIZATION_RULE_TYPES {
        registry.register_type(namespaces::AuthorizationRules(kind));
    }
    registry.register_type(entities::Queues);
    registry.register_type(entities::Topics);
    registry.register_type(entities::Subscriptions);
    registry.register_type(entities::Rules);
    for action in ["checkNameAvailability", "CheckNameSpaceAvailability"] {
        registry.register_provider_action(
            &format!("Microsoft.ServiceBus/{action}"),
            Arc::new(namespaces::check_name_availability),
        );
    }
    geo::register(registry);
    migration::register(registry);
    registry.register_type(emulator_core::arm::CreatedByNetwork("Microsoft.ServiceBus/namespaces/privateEndpointConnections"));
    registry.register_type(emulator_core::arm::CreatedByNetwork("Microsoft.ServiceBus/namespaces/networkSecurityPerimeterConfigurations"));
    registry.register_scaffold(&scaffold::PROVIDER);
}
