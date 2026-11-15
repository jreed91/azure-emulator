Issue-Kind: resource-type
Issue-Service: services/cosmosdb
Issue-Key: Microsoft.DocumentDB/databaseAccounts/mongodbDatabases/collections/throughputSettings

# Implement the remaining operations of Microsoft.DocumentDB/databaseAccounts/mongodbDatabases/collections/throughputSettings

`Microsoft.DocumentDB/databaseAccounts/mongodbDatabases/collections/throughputSettings` is served from its schema, but 4 of its 6 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/mongodbDatabases/{databaseName}/collections/{collectionName}/throughputSettings/default/migrateToAutoscale` | `MongoDBResources_MigrateMongoDBCollectionToAutoscale` |
| `501` | POST | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/mongodbDatabases/{databaseName}/collections/{collectionName}/throughputSettings/default/migrateToManualThroughput` | `MongoDBResources_MigrateMongoDBCollectionToManualThroughput` |
| `501` | POST | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/mongodbDatabases/{databaseName}/collections/{collectionName}/throughputSettings/default/redistributeThroughput` | `MongoDBResources_MongoDBContainerRedistributeThroughput` |
| `501` | POST | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/mongodbDatabases/{databaseName}/collections/{collectionName}/throughputSettings/default/retrieveThroughputDistribution` | `MongoDBResources_MongoDBContainerRetrieveThroughputDistribution` |
| `generic` | GET | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/mongodbDatabases/{databaseName}/collections/{collectionName}/throughputSettings/default` | `MongoDBResources_GetMongoDBCollectionThroughput` |
| `generic` | PUT | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/mongodbDatabases/{databaseName}/collections/{collectionName}/throughputSettings/default` | `MongoDBResources_UpdateMongoDBCollectionThroughput` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/cosmosdb/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/cosmosdb/CONTRACT.md`, section `Microsoft.DocumentDB/databaseAccounts/mongodbDatabases/collections/throughputSettings`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
