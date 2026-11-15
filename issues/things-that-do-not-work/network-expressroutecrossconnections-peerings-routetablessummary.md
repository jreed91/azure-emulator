Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/expressRouteCrossConnections/peerings/routeTablesSummary

# Implement Microsoft.Network/expressRouteCrossConnections/peerings/routeTablesSummary

Every operation of `Microsoft.Network/expressRouteCrossConnections/peerings/routeTablesSummary` answers `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Network/expressRouteCrossConnections/{crossConnectionName}/peerings/{peeringName}/routeTablesSummary/{devicePath}` | `ExpressRouteCrossConnections_ListRoutesTableSummary` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/expressRouteCrossConnections/peerings/routeTablesSummary`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
