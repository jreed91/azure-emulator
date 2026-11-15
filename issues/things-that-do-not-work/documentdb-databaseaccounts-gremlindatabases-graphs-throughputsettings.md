Issue-Kind: resource-type
Issue-Service: services/cosmosdb
Issue-Key: Microsoft.DocumentDB/databaseAccounts/gremlinDatabases/graphs/throughputSettings

# Implement the remaining operations of Microsoft.DocumentDB/databaseAccounts/gremlinDatabases/graphs/throughputSettings

`Microsoft.DocumentDB/databaseAccounts/gremlinDatabases/graphs/throughputSettings` is served from its schema, but 2 of its 4 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/gremlinDatabases/{databaseName}/graphs/{graphName}/throughputSettings/default/migrateToAutoscale` | `GremlinResources_MigrateGremlinGraphToAutoscale` |
| `501` | POST | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/gremlinDatabases/{databaseName}/graphs/{graphName}/throughputSettings/default/migrateToManualThroughput` | `GremlinResources_MigrateGremlinGraphToManualThroughput` |
| `generic` | GET | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/gremlinDatabases/{databaseName}/graphs/{graphName}/throughputSettings/default` | `GremlinResources_GetGremlinGraphThroughput` |
| `generic` | PUT | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/gremlinDatabases/{databaseName}/graphs/{graphName}/throughputSettings/default` | `GremlinResources_UpdateGremlinGraphThroughput` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/cosmosdb/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/cosmosdb/CONTRACT.md`, section `Microsoft.DocumentDB/databaseAccounts/gremlinDatabases/graphs/throughputSettings`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
