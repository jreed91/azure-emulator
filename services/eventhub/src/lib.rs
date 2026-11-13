//! `Microsoft.EventHub`: namespaces, event hubs, consumer groups and
//! authorization rules on Resource Manager. Events are sent and received
//! over AMQP through the Service Bus crate's broker, which serves both kinds
//! of namespace (see `service_servicebus::broker::eventhubs`).
//!
//! Namespaces, network rule sets and authorization rules are the Service Bus
//! handlers with the Event Hubs types: names are unique across both
//! providers, as on Azure, and keys come from the same store.
//!
//! Every other resource type in the provider's API specification is mounted
//! from [`scaffold::PROVIDER`] and answers `501` until it is implemented.

mod hubs;
pub mod scaffold;

use std::sync::Arc;

use emulator_core::arm::ArmRegistry;
use service_servicebus::namespaces::{self, AuthorizationRules, Namespaces, NetworkRuleSets, EVENT_HUBS};

/// Mounts the provider on Resource Manager.
pub fn register(registry: &mut ArmRegistry) {
    registry.register_type(Namespaces(&EVENT_HUBS));
    registry.register_type(NetworkRuleSets(&EVENT_HUBS));
    registry.register_type(AuthorizationRules("Microsoft.EventHub/namespaces/authorizationRules"));
    registry.register_type(AuthorizationRules("Microsoft.EventHub/namespaces/eventhubs/authorizationRules"));
    registry.register_type(hubs::EventHubs);
    registry.register_type(hubs::ConsumerGroups);
    registry.register_provider_action("Microsoft.EventHub/checkNameAvailability", Arc::new(namespaces::check_name_availability));
    registry.register_scaffold(&scaffold::PROVIDER);
}
