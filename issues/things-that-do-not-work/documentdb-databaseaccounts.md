Issue-Kind: resource-type
Issue-Service: services/cosmosdb
Issue-Key: Microsoft.DocumentDB/databaseAccounts

# Implement the remaining operations of Microsoft.DocumentDB/databaseAccounts

`Microsoft.DocumentDB/databaseAccounts` is served by a handler, but 3 of its 13 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/failoverPriorityChange` | `DatabaseAccounts_FailoverPriorityChange` |
| `501` | POST | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/offlineRegion` | `DatabaseAccounts_OfflineRegion` |
| `501` | POST | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/onlineRegion` | `DatabaseAccounts_OnlineRegion` |
| `ok` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.DocumentDB/databaseAccounts` | `DatabaseAccounts_List` |
| `ok` | GET | `.../Microsoft.DocumentDB/databaseAccounts` | `DatabaseAccounts_ListByResourceGroup` |
| `ok` | DELETE | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}` | `DatabaseAccounts_Delete` |
| `ok` | GET | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}` | `DatabaseAccounts_Get` |
| `ok` | PATCH | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}` | `DatabaseAccounts_Update` |
| `ok` | PUT | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}` | `DatabaseAccounts_CreateOrUpdate` |
| `ok` | POST | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/listConnectionStrings` | `DatabaseAccounts_ListConnectionStrings` |
| `ok` | POST | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/listKeys` | `DatabaseAccounts_ListKeys` |
| `ok` | POST | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/readonlykeys` | `DatabaseAccounts_ListReadOnlyKeys` |
| `ok` | POST | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/regenerateKey` | `DatabaseAccounts_RegenerateKey` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/cosmosdb/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/cosmosdb/CONTRACT.md`, section `Microsoft.DocumentDB/databaseAccounts`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
