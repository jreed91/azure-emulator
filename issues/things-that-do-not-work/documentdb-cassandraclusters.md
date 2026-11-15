Issue-Kind: resource-type
Issue-Service: services/cosmosdb
Issue-Key: Microsoft.DocumentDB/cassandraClusters

# Implement the remaining operations of Microsoft.DocumentDB/cassandraClusters

`Microsoft.DocumentDB/cassandraClusters` is served from its schema, but 6 of its 12 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.DocumentDB/cassandraClusters/{clusterName}/deallocate` | `CassandraClusters_Deallocate` |
| `501` | POST | `.../Microsoft.DocumentDB/cassandraClusters/{clusterName}/fetchNodeStatus` | `CassandraClusters_FetchNodeStatus` |
| `501` | POST | `.../Microsoft.DocumentDB/cassandraClusters/{clusterName}/invokeCommand` | `CassandraClusters_InvokeCommand` |
| `501` | POST | `.../Microsoft.DocumentDB/cassandraClusters/{clusterName}/invokeCommandAsync` | `CassandraClusters_InvokeCommandAsync` |
| `501` | POST | `.../Microsoft.DocumentDB/cassandraClusters/{clusterName}/repair` | `CassandraClusters_RequestRepair` |
| `501` | POST | `.../Microsoft.DocumentDB/cassandraClusters/{clusterName}/start` | `CassandraClusters_Start` |
| `generic` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.DocumentDB/cassandraClusters` | `CassandraClusters_ListBySubscription` |
| `generic` | GET | `.../Microsoft.DocumentDB/cassandraClusters` | `CassandraClusters_ListByResourceGroup` |
| `generic` | DELETE | `.../Microsoft.DocumentDB/cassandraClusters/{clusterName}` | `CassandraClusters_Delete` |
| `generic` | GET | `.../Microsoft.DocumentDB/cassandraClusters/{clusterName}` | `CassandraClusters_Get` |
| `generic` | PATCH | `.../Microsoft.DocumentDB/cassandraClusters/{clusterName}` | `CassandraClusters_Update` |
| `generic` | PUT | `.../Microsoft.DocumentDB/cassandraClusters/{clusterName}` | `CassandraClusters_CreateUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/cosmosdb/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/cosmosdb/CONTRACT.md`, section `Microsoft.DocumentDB/cassandraClusters`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
