Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/virtualHubs

# Implement the remaining operations of Microsoft.Network/virtualHubs

`Microsoft.Network/virtualHubs` is served from its schema, but 3 of its 9 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Network/virtualHubs/{virtualHubName}/effectiveRoutes` | `VirtualHubs_GetEffectiveVirtualHubRoutes` |
| `501` | POST | `.../Microsoft.Network/virtualHubs/{virtualHubName}/inboundRoutes` | `VirtualHubs_GetInboundRoutes` |
| `501` | POST | `.../Microsoft.Network/virtualHubs/{virtualHubName}/outboundRoutes` | `VirtualHubs_GetOutboundRoutes` |
| `generic` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.Network/virtualHubs` | `VirtualHubs_List` |
| `generic` | GET | `.../Microsoft.Network/virtualHubs` | `VirtualHubs_ListByResourceGroup` |
| `generic` | DELETE | `.../Microsoft.Network/virtualHubs/{virtualHubName}` | `VirtualHubs_Delete` |
| `generic` | GET | `.../Microsoft.Network/virtualHubs/{virtualHubName}` | `VirtualHubs_Get` |
| `generic` | PATCH | `.../Microsoft.Network/virtualHubs/{virtualHubName}` | `VirtualHubs_UpdateTags` |
| `generic` | PUT | `.../Microsoft.Network/virtualHubs/{virtualHubName}` | `VirtualHubs_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/virtualHubs`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
