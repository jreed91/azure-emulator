//! `Microsoft.Network` resource provider.
//!
//! Every resource type in the provider's API specification is mounted from
//! [`scaffold::PROVIDER`] and answers `501` until it is implemented. To implement a
//! type, write a `ResourceTypeHandler` for it and register it here, before the
//! scaffold. `CONTRACT.md` describes what the specification says about each type.

pub mod scaffold;

use emulator_core::arm::ArmRegistry;

/// Mounts the provider on Resource Manager.
pub fn register(registry: &mut ArmRegistry) {
    registry.register_generic_remaining(&scaffold::PROVIDER, GENERIC_EXCEPTIONS);
    registry.register_scaffold(&scaffold::PROVIDER);
}

/// The configuration-shaped types that are not served from their schema, and why.
const GENERIC_EXCEPTIONS: &[(&str, &str)] = &[
    ("Microsoft.Network/privateLinkServices/privateEndpointConnections", "created by a private endpoint connecting to the service, not by a PUT"),
    ("Microsoft.Network/networkWatchers/packetCaptures", "starts a capture; storing the setting would report one that is not running"),
    ("Microsoft.Network/vpnGateways/vpnConnections/vpnLinkConnections/sharedKeys", "its only example names a link connection no example creates, so nothing shows it works"),
];
