Issue-Kind: resource-type
Issue-Service: services/cosmosdb
Issue-Key: Microsoft.DocumentDB/databaseAccounts/sqlDatabases/clientEncryptionKeys

# Implement Microsoft.DocumentDB/databaseAccounts/sqlDatabases/clientEncryptionKeys

Every operation of `Microsoft.DocumentDB/databaseAccounts/sqlDatabases/clientEncryptionKeys` answers `501`.

## Why it is kept at 501

The type has a schema it could be served from, and is not: keys the data plane would have to encrypt with, and does not.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | GET | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/sqlDatabases/{databaseName}/clientEncryptionKeys` | `SqlResources_ListClientEncryptionKeys` |
| `501` | GET | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/sqlDatabases/{databaseName}/clientEncryptionKeys/{clientEncryptionKeyName}` | `SqlResources_GetClientEncryptionKey` |
| `501` | PUT | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/sqlDatabases/{databaseName}/clientEncryptionKeys/{clientEncryptionKeyName}` | `SqlResources_CreateUpdateClientEncryptionKey` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/cosmosdb/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/cosmosdb/CONTRACT.md`, section `Microsoft.DocumentDB/databaseAccounts/sqlDatabases/clientEncryptionKeys`, says what the specification requires of the type's name and body.
- Remove the type from `GENERIC_EXCEPTIONS` in `services/cosmosdb/src/lib.rs` once a handler serves it.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
