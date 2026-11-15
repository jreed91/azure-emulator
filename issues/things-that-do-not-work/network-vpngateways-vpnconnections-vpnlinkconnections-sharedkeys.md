Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/vpnGateways/vpnConnections/vpnLinkConnections/sharedKeys

# Implement Microsoft.Network/vpnGateways/vpnConnections/vpnLinkConnections/sharedKeys

Every operation of `Microsoft.Network/vpnGateways/vpnConnections/vpnLinkConnections/sharedKeys` answers `501`.

## Why it is kept at 501

The type has a schema it could be served from, and is not: its only example names a link connection no example creates, so nothing shows it works.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | GET | `.../Microsoft.Network/vpnGateways/{gatewayName}/vpnConnections/{connectionName}/vpnLinkConnections/{linkConnectionName}/sharedKeys` | `VpnLinkConnections_GetAllSharedKeys` |
| `501` | GET | `.../Microsoft.Network/vpnGateways/{gatewayName}/vpnConnections/{connectionName}/vpnLinkConnections/{linkConnectionName}/sharedKeys/default` | `VpnLinkConnections_GetDefaultSharedKey` |
| `501` | PUT | `.../Microsoft.Network/vpnGateways/{gatewayName}/vpnConnections/{connectionName}/vpnLinkConnections/{linkConnectionName}/sharedKeys/default` | `VpnLinkConnections_SetOrInitDefaultSharedKey` |
| `501` | POST | `.../Microsoft.Network/vpnGateways/{gatewayName}/vpnConnections/{connectionName}/vpnLinkConnections/{linkConnectionName}/sharedKeys/default/listSharedKey` | `VpnLinkConnections_ListDefaultSharedKey` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/vpnGateways/vpnConnections/vpnLinkConnections/sharedKeys`, says what the specification requires of the type's name and body.
- Remove the type from `GENERIC_EXCEPTIONS` in `services/network/src/lib.rs` once a handler serves it.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
