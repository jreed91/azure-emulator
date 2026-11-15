Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/p2svpnGateways

# Implement the remaining operations of Microsoft.Network/p2svpnGateways

`Microsoft.Network/p2svpnGateways` is served from its schema, but 5 of its 11 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Network/p2svpnGateways/{gatewayName}/generatevpnprofile` | `P2sVpnGateways_GenerateVpnProfile` |
| `501` | POST | `.../Microsoft.Network/p2svpnGateways/{gatewayName}/getP2sVpnConnectionHealth` | `P2sVpnGateways_GetP2sVpnConnectionHealth` |
| `501` | POST | `.../Microsoft.Network/p2svpnGateways/{gatewayName}/getP2sVpnConnectionHealthDetailed` | `P2sVpnGateways_GetP2sVpnConnectionHealthDetailed` |
| `501` | POST | `.../Microsoft.Network/p2svpnGateways/{gatewayName}/reset` | `P2SVpnGateways_Reset` |
| `501` | POST | `.../Microsoft.Network/p2svpnGateways/{p2sVpnGatewayName}/disconnectP2sVpnConnections` | `P2sVpnGateways_DisconnectP2sVpnConnections` |
| `generic` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.Network/p2svpnGateways` | `P2sVpnGateways_List` |
| `generic` | GET | `.../Microsoft.Network/p2svpnGateways` | `P2sVpnGateways_ListByResourceGroup` |
| `generic` | DELETE | `.../Microsoft.Network/p2svpnGateways/{gatewayName}` | `P2sVpnGateways_Delete` |
| `generic` | GET | `.../Microsoft.Network/p2svpnGateways/{gatewayName}` | `P2sVpnGateways_Get` |
| `generic` | PATCH | `.../Microsoft.Network/p2svpnGateways/{gatewayName}` | `P2sVpnGateways_UpdateTags` |
| `generic` | PUT | `.../Microsoft.Network/p2svpnGateways/{gatewayName}` | `P2sVpnGateways_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/p2svpnGateways`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
