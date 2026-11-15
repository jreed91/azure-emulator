Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/networkManagers

# Implement the remaining operations of Microsoft.Network/networkManagers

`Microsoft.Network/networkManagers` is served from its schema, but 6 of its 12 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Network/networkManagers/{networkManagerName}/commit` | `NetworkManagerCommits_Post` |
| `501` | POST | `.../Microsoft.Network/networkManagers/{networkManagerName}/listActiveConnectivityConfigurations` | `ListActiveConnectivityConfigurations` |
| `501` | POST | `.../Microsoft.Network/networkManagers/{networkManagerName}/listActiveSecurityAdminRules` | `ListActiveSecurityAdminRules` |
| `501` | POST | `.../Microsoft.Network/networkManagers/{networkManagerName}/listActiveSecurityUserRules` | `ListActiveSecurityUserRules` |
| `501` | POST | `.../Microsoft.Network/networkManagers/{networkManagerName}/listDeploymentStatus` | `NetworkManagerDeploymentStatus_List` |
| `501` | POST | `.../Microsoft.Network/networkManagers/{networkManagerName}/listEffectiveVirtualNetworks` | `EffectiveVirtualNetworks_ListByNetworkManager` |
| `generic` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.Network/networkManagers` | `NetworkManagers_ListBySubscription` |
| `generic` | GET | `.../Microsoft.Network/networkManagers` | `NetworkManagers_List` |
| `generic` | DELETE | `.../Microsoft.Network/networkManagers/{networkManagerName}` | `NetworkManagers_Delete` |
| `generic` | GET | `.../Microsoft.Network/networkManagers/{networkManagerName}` | `NetworkManagers_Get` |
| `generic` | PATCH | `.../Microsoft.Network/networkManagers/{networkManagerName}` | `NetworkManagers_Patch` |
| `generic` | PUT | `.../Microsoft.Network/networkManagers/{networkManagerName}` | `NetworkManagers_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/networkManagers`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
