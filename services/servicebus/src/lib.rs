//! `Microsoft.ServiceBus` resource provider.
//!
//! Every resource type in the provider's API specification is mounted from
//! [`scaffold::PROVIDER`] and answers `501` until it is implemented. To implement a
//! type, write a `ResourceTypeHandler` for it and register it here, before the
//! scaffold. `CONTRACT.md` describes what the specification says about each type.

pub mod scaffold;

use emulator_core::arm::ArmRegistry;

/// Mounts the provider on Resource Manager.
pub fn register(registry: &mut ArmRegistry) {
    registry.register_scaffold(&scaffold::PROVIDER);
}
