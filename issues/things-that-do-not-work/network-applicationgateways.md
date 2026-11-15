Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/applicationGateways

# Implement the remaining operations of Microsoft.Network/applicationGateways

`Microsoft.Network/applicationGateways` is served from its schema, but 4 of its 10 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Network/applicationGateways/{applicationGatewayName}/backendhealth` | `ApplicationGateways_BackendHealth` |
| `501` | POST | `.../Microsoft.Network/applicationGateways/{applicationGatewayName}/getBackendHealthOnDemand` | `ApplicationGateways_BackendHealthOnDemand` |
| `501` | POST | `.../Microsoft.Network/applicationGateways/{applicationGatewayName}/start` | `ApplicationGateways_Start` |
| `501` | POST | `.../Microsoft.Network/applicationGateways/{applicationGatewayName}/stop` | `ApplicationGateways_Stop` |
| `generic` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.Network/applicationGateways` | `ApplicationGateways_ListAll` |
| `generic` | GET | `.../Microsoft.Network/applicationGateways` | `ApplicationGateways_List` |
| `generic` | DELETE | `.../Microsoft.Network/applicationGateways/{applicationGatewayName}` | `ApplicationGateways_Delete` |
| `generic` | GET | `.../Microsoft.Network/applicationGateways/{applicationGatewayName}` | `ApplicationGateways_Get` |
| `generic` | PATCH | `.../Microsoft.Network/applicationGateways/{applicationGatewayName}` | `ApplicationGateways_UpdateTags` |
| `generic` | PUT | `.../Microsoft.Network/applicationGateways/{applicationGatewayName}` | `ApplicationGateways_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/applicationGateways`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
