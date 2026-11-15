Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/vpnGateways/vpnConnections

# Implement the remaining operations of Microsoft.Network/vpnGateways/vpnConnections

`Microsoft.Network/vpnGateways/vpnConnections` is served from its schema, but 2 of its 6 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Network/vpnGateways/{gatewayName}/vpnConnections/{vpnConnectionName}/startpacketcapture` | `VpnConnections_StartPacketCapture` |
| `501` | POST | `.../Microsoft.Network/vpnGateways/{gatewayName}/vpnConnections/{vpnConnectionName}/stoppacketcapture` | `VpnConnections_StopPacketCapture` |
| `generic` | GET | `.../Microsoft.Network/vpnGateways/{gatewayName}/vpnConnections` | `VpnConnections_ListByVpnGateway` |
| `generic` | DELETE | `.../Microsoft.Network/vpnGateways/{gatewayName}/vpnConnections/{connectionName}` | `VpnConnections_Delete` |
| `generic` | GET | `.../Microsoft.Network/vpnGateways/{gatewayName}/vpnConnections/{connectionName}` | `VpnConnections_Get` |
| `generic` | PUT | `.../Microsoft.Network/vpnGateways/{gatewayName}/vpnConnections/{connectionName}` | `VpnConnections_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/vpnGateways/vpnConnections`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
