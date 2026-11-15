Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/virtualNetworks

# Implement the remaining operations of Microsoft.Network/virtualNetworks

`Microsoft.Network/virtualNetworks` is served from its schema, but 4 of its 10 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Network/virtualNetworks/{virtualNetworkName}/ddosProtectionStatus` | `VirtualNetworks_ListDdosProtectionStatus` |
| `501` | POST | `.../Microsoft.Network/virtualNetworks/{virtualNetworkName}/listNetworkManagerEffectiveConnectivityConfigurations` | `ListNetworkManagerEffectiveConnectivityConfigurations` |
| `501` | POST | `.../Microsoft.Network/virtualNetworks/{virtualNetworkName}/listNetworkManagerEffectiveSecurityAdminRules` | `ListNetworkManagerEffectiveSecurityAdminRules` |
| `501` | POST | `.../Microsoft.Network/virtualNetworks/{virtualNetworkName}/moveIpConfigurations` | `VirtualNetworks_MoveIpConfigurations` |
| `generic` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.Network/virtualNetworks` | `VirtualNetworks_ListAll` |
| `generic` | GET | `.../Microsoft.Network/virtualNetworks` | `VirtualNetworks_List` |
| `generic` | DELETE | `.../Microsoft.Network/virtualNetworks/{virtualNetworkName}` | `VirtualNetworks_Delete` |
| `generic` | GET | `.../Microsoft.Network/virtualNetworks/{virtualNetworkName}` | `VirtualNetworks_Get` |
| `generic` | PATCH | `.../Microsoft.Network/virtualNetworks/{virtualNetworkName}` | `VirtualNetworks_UpdateTags` |
| `generic` | PUT | `.../Microsoft.Network/virtualNetworks/{virtualNetworkName}` | `VirtualNetworks_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/virtualNetworks`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
