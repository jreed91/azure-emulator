Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/virtualNetworkGateways

# Implement the remaining operations of Microsoft.Network/virtualNetworkGateways

`Microsoft.Network/virtualNetworkGateways` is served from its schema, but 27 of its 32 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/abortMigration` | `VirtualNetworkGateways_InvokeAbortMigration` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/commitMigration` | `VirtualNetworkGateways_InvokeCommitMigration` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/disconnectVirtualNetworkGatewayVpnConnections` | `VirtualNetworkGateways_DisconnectVirtualNetworkGatewayVpnConnections` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/executeMigration` | `VirtualNetworkGateways_InvokeExecuteMigration` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/generatevpnclientpackage` | `VirtualNetworkGateways_Generatevpnclientpackage` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/generatevpnprofile` | `VirtualNetworkGateways_GenerateVpnProfile` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/getAdvertisedRoutes` | `VirtualNetworkGateways_GetAdvertisedRoutes` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/getBgpPeerStatus` | `VirtualNetworkGateways_GetBgpPeerStatus` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/getEffectiveRoutes` | `VirtualNetworkGateways_GetEffectiveRoutes` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/getFailoverAllTestsDetails` | `VirtualNetworkGateways_GetFailoverAllTestDetails` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/getFailoverSingleTestDetails` | `VirtualNetworkGateways_GetFailoverSingleTestDetails` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/getLearnedRoutes` | `VirtualNetworkGateways_GetLearnedRoutes` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/getResiliencyInformation` | `VirtualNetworkGateways_GetResiliencyInformation` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/getRoutesInformation` | `VirtualNetworkGateways_GetRoutesInformation` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/getVpnClientConnectionHealth` | `VirtualNetworkGateways_GetVpnclientConnectionHealth` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/getvpnclientipsecparameters` | `VirtualNetworkGateways_GetVpnclientIpsecParameters` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/getvpnprofilepackageurl` | `VirtualNetworkGateways_GetVpnProfilePackageUrl` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/listRadiusSecrets` | `VirtualNetworkGateways_ListRadiusSecrets` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/prepareMigration` | `VirtualNetworkGateways_InvokePrepareMigration` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/reset` | `VirtualNetworkGateways_Reset` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/resetvpnclientsharedkey` | `VirtualNetworkGateways_ResetVpnClientSharedKey` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/setvpnclientipsecparameters` | `VirtualNetworkGateways_SetVpnclientIpsecParameters` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/startPacketCapture` | `VirtualNetworkGateways_StartPacketCapture` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/startSiteFailoverTest` | `VirtualNetworkGateways_StartExpressRouteSiteFailoverSimulation` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/stopPacketCapture` | `VirtualNetworkGateways_StopPacketCapture` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/stopSiteFailoverTest` | `VirtualNetworkGateways_StopExpressRouteSiteFailoverSimulation` |
| `501` | POST | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}/supportedvpndevices` | `VirtualNetworkGateways_SupportedVpnDevices` |
| `generic` | GET | `.../Microsoft.Network/virtualNetworkGateways` | `VirtualNetworkGateways_List` |
| `generic` | DELETE | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}` | `VirtualNetworkGateways_Delete` |
| `generic` | GET | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}` | `VirtualNetworkGateways_Get` |
| `generic` | PATCH | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}` | `VirtualNetworkGateways_UpdateTags` |
| `generic` | PUT | `.../Microsoft.Network/virtualNetworkGateways/{virtualNetworkGatewayName}` | `VirtualNetworkGateways_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/virtualNetworkGateways`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
