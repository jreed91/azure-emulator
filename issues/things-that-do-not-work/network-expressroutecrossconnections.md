Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/expressRouteCrossConnections

# Implement the remaining operations of Microsoft.Network/expressRouteCrossConnections

`Microsoft.Network/expressRouteCrossConnections` is served from its schema, but 8 of its 13 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Network/expressRouteCrossConnections/{crossConnectionName}/commitCircuitMigration` | `ExpressRouteCrossConnections_CommitCircuitMigration` |
| `501` | POST | `.../Microsoft.Network/expressRouteCrossConnections/{crossConnectionName}/getCircuitMigrationInfo` | `ExpressRouteCrossConnections_GetCircuitMigrationInfo` |
| `501` | POST | `.../Microsoft.Network/expressRouteCrossConnections/{crossConnectionName}/migrateCircuit` | `ExpressRouteCrossConnections_MigrateCircuit` |
| `501` | POST | `.../Microsoft.Network/expressRouteCrossConnections/{crossConnectionName}/prepareCircuitMigration` | `ExpressRouteCrossConnections_PrepareCircuitMigration` |
| `501` | POST | `.../Microsoft.Network/expressRouteCrossConnections/{crossConnectionName}/restoreBgpForCircuitMigration` | `ExpressRouteCrossConnections_RestoreBgpForCircuitMigration` |
| `501` | POST | `.../Microsoft.Network/expressRouteCrossConnections/{crossConnectionName}/rollbackCircuitMigration` | `ExpressRouteCrossConnections_RollbackCircuitMigration` |
| `501` | POST | `.../Microsoft.Network/expressRouteCrossConnections/{crossConnectionName}/shutDownBgpForCircuitMigration` | `ExpressRouteCrossConnections_ShutDownBgpForCircuitMigration` |
| `501` | POST | `.../Microsoft.Network/expressRouteCrossConnections/{crossConnectionName}/validateCircuitMigration` | `ExpressRouteCrossConnections_ValidateCircuitMigration` |
| `generic` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.Network/expressRouteCrossConnections` | `ExpressRouteCrossConnections_List` |
| `generic` | GET | `.../Microsoft.Network/expressRouteCrossConnections` | `ExpressRouteCrossConnections_ListByResourceGroup` |
| `generic` | GET | `.../Microsoft.Network/expressRouteCrossConnections/{crossConnectionName}` | `ExpressRouteCrossConnections_Get` |
| `generic` | PATCH | `.../Microsoft.Network/expressRouteCrossConnections/{crossConnectionName}` | `ExpressRouteCrossConnections_UpdateTags` |
| `generic` | PUT | `.../Microsoft.Network/expressRouteCrossConnections/{crossConnectionName}` | `ExpressRouteCrossConnections_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/expressRouteCrossConnections`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
