Issue-Kind: resource-type
Issue-Service: services/cosmosdb
Issue-Key: Microsoft.DocumentDB/locations/softDeletedDatabaseAccounts

# Implement Microsoft.DocumentDB/locations/softDeletedDatabaseAccounts

Every operation of `Microsoft.DocumentDB/locations/softDeletedDatabaseAccounts` answers `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | GET | `.../Microsoft.DocumentDB/locations/{location}/softDeletedDatabaseAccounts` | `SoftDeletedDatabaseAccounts_ListByResourceGroupAndLocation` |
| `501` | GET | `.../Microsoft.DocumentDB/locations/{location}/softDeletedDatabaseAccounts/{accountName}` | `SoftDeletedDatabaseAccounts_Get` |
| `501` | DELETE | `.../Microsoft.DocumentDB/locations/{location}/softDeletedDatabaseAccounts/{accountName}?softDeleteActionKind=PermanentDeleteResource` | `SoftDeletedDatabaseAccounts_Purge` |
| `501` | DELETE | `.../Microsoft.DocumentDB/locations/{location}/softDeletedDatabaseAccounts/{accountName}?softDeleteActionKind=RestoreSoftDeletedResource` | `SoftDeletedDatabaseAccounts_Restore` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/cosmosdb/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/cosmosdb/CONTRACT.md`, section `Microsoft.DocumentDB/locations/softDeletedDatabaseAccounts`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
