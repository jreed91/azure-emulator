Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/networkWatchers

# Implement the remaining operations of Microsoft.Network/networkWatchers

`Microsoft.Network/networkWatchers` is served from its schema, but 12 of its 18 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/availableProvidersList` | `NetworkWatchers_ListAvailableProviders` |
| `501` | POST | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/azureReachabilityReport` | `NetworkWatchers_GetAzureReachabilityReport` |
| `501` | POST | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/configureFlowLog` | `NetworkWatchers_SetFlowLogConfiguration` |
| `501` | POST | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/connectivityCheck` | `NetworkWatchers_CheckConnectivity` |
| `501` | POST | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/ipFlowVerify` | `NetworkWatchers_VerifyIPFlow` |
| `501` | POST | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/networkConfigurationDiagnostic` | `NetworkWatchers_GetNetworkConfigurationDiagnostic` |
| `501` | POST | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/nextHop` | `NetworkWatchers_GetNextHop` |
| `501` | POST | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/queryFlowLogStatus` | `NetworkWatchers_GetFlowLogStatus` |
| `501` | POST | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/queryTroubleshootResult` | `NetworkWatchers_GetTroubleshootingResult` |
| `501` | POST | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/securityGroupView` | `NetworkWatchers_GetVMSecurityRules` |
| `501` | POST | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/topology` | `NetworkWatchers_GetTopology` |
| `501` | POST | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/troubleshoot` | `NetworkWatchers_GetTroubleshooting` |
| `generic` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.Network/networkWatchers` | `NetworkWatchers_ListAll` |
| `generic` | GET | `.../Microsoft.Network/networkWatchers` | `NetworkWatchers_List` |
| `generic` | DELETE | `.../Microsoft.Network/networkWatchers/{networkWatcherName}` | `NetworkWatchers_Delete` |
| `generic` | GET | `.../Microsoft.Network/networkWatchers/{networkWatcherName}` | `NetworkWatchers_Get` |
| `generic` | PATCH | `.../Microsoft.Network/networkWatchers/{networkWatcherName}` | `NetworkWatchers_UpdateTags` |
| `generic` | PUT | `.../Microsoft.Network/networkWatchers/{networkWatcherName}` | `NetworkWatchers_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/networkWatchers`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
