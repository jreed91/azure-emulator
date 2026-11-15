Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/expressRouteCircuits

# Implement the remaining operations of Microsoft.Network/expressRouteCircuits

`Microsoft.Network/expressRouteCircuits` is served from its schema, but 4 of its 10 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Network/expressRouteCircuits/{circuitName}/getCircuitLinkFailoverAllTestsDetails` | `ExpressRouteCircuits_GetCircuitLinkFailoverAllTestsDetails` |
| `501` | POST | `.../Microsoft.Network/expressRouteCircuits/{circuitName}/getCircuitLinkFailoverSingleTestDetails` | `ExpressRouteCircuits_GetCircuitLinkFailoverSingleTestDetails` |
| `501` | POST | `.../Microsoft.Network/expressRouteCircuits/{circuitName}/startCircuitLinkFailoverTest` | `ExpressRouteCircuits_StartCircuitLinkFailoverTest` |
| `501` | POST | `.../Microsoft.Network/expressRouteCircuits/{circuitName}/stopCircuitLinkFailoverTest` | `ExpressRouteCircuits_StopCircuitLinkFailoverTest` |
| `generic` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.Network/expressRouteCircuits` | `ExpressRouteCircuits_ListAll` |
| `generic` | GET | `.../Microsoft.Network/expressRouteCircuits` | `ExpressRouteCircuits_List` |
| `generic` | DELETE | `.../Microsoft.Network/expressRouteCircuits/{circuitName}` | `ExpressRouteCircuits_Delete` |
| `generic` | GET | `.../Microsoft.Network/expressRouteCircuits/{circuitName}` | `ExpressRouteCircuits_Get` |
| `generic` | PATCH | `.../Microsoft.Network/expressRouteCircuits/{circuitName}` | `ExpressRouteCircuits_UpdateTags` |
| `generic` | PUT | `.../Microsoft.Network/expressRouteCircuits/{circuitName}` | `ExpressRouteCircuits_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/expressRouteCircuits`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
