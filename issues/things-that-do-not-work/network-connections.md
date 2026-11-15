Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/connections

# Implement the remaining operations of Microsoft.Network/connections

`Microsoft.Network/connections` is served from its schema, but 5 of its 10 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Network/connections/{virtualNetworkGatewayConnectionName}/getikesas` | `VirtualNetworkGatewayConnections_GetIkeSas` |
| `501` | POST | `.../Microsoft.Network/connections/{virtualNetworkGatewayConnectionName}/resetconnection` | `VirtualNetworkGatewayConnections_ResetConnection` |
| `501` | POST | `.../Microsoft.Network/connections/{virtualNetworkGatewayConnectionName}/startPacketCapture` | `VirtualNetworkGatewayConnections_StartPacketCapture` |
| `501` | POST | `.../Microsoft.Network/connections/{virtualNetworkGatewayConnectionName}/stopPacketCapture` | `VirtualNetworkGatewayConnections_StopPacketCapture` |
| `501` | POST | `.../Microsoft.Network/connections/{virtualNetworkGatewayConnectionName}/vpndeviceconfigurationscript` | `VirtualNetworkGateways_VpnDeviceConfigurationScript` |
| `generic` | GET | `.../Microsoft.Network/connections` | `VirtualNetworkGatewayConnections_List` |
| `generic` | DELETE | `.../Microsoft.Network/connections/{virtualNetworkGatewayConnectionName}` | `VirtualNetworkGatewayConnections_Delete` |
| `generic` | GET | `.../Microsoft.Network/connections/{virtualNetworkGatewayConnectionName}` | `VirtualNetworkGatewayConnections_Get` |
| `generic` | PATCH | `.../Microsoft.Network/connections/{virtualNetworkGatewayConnectionName}` | `VirtualNetworkGatewayConnections_UpdateTags` |
| `generic` | PUT | `.../Microsoft.Network/connections/{virtualNetworkGatewayConnectionName}` | `VirtualNetworkGatewayConnections_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/connections`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
