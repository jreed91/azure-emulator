Issue-Kind: resource-type
Issue-Service: services/cosmosdb
Issue-Key: Microsoft.DocumentDB/mongoClusters

# Implement the remaining operations of Microsoft.DocumentDB/mongoClusters

`Microsoft.DocumentDB/mongoClusters` is served from its schema, but 1 of its 7 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.DocumentDB/mongoClusters/{mongoClusterName}/listConnectionStrings` | `MongoClusters_ListConnectionStrings` |
| `generic` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.DocumentDB/mongoClusters` | `MongoClusters_List` |
| `generic` | GET | `.../Microsoft.DocumentDB/mongoClusters` | `MongoClusters_ListByResourceGroup` |
| `generic` | DELETE | `.../Microsoft.DocumentDB/mongoClusters/{mongoClusterName}` | `MongoClusters_Delete` |
| `generic` | GET | `.../Microsoft.DocumentDB/mongoClusters/{mongoClusterName}` | `MongoClusters_Get` |
| `generic` | PATCH | `.../Microsoft.DocumentDB/mongoClusters/{mongoClusterName}` | `MongoClusters_Update` |
| `generic` | PUT | `.../Microsoft.DocumentDB/mongoClusters/{mongoClusterName}` | `MongoClusters_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/cosmosdb/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/cosmosdb/CONTRACT.md`, section `Microsoft.DocumentDB/mongoClusters`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
