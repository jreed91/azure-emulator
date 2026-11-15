Issue-Kind: resource-type
Issue-Service: services/cosmosdb
Issue-Key: Microsoft.DocumentDB/databaseAccounts/mongoMIRoleAssignments

# Implement Microsoft.DocumentDB/databaseAccounts/mongoMIRoleAssignments

Every operation of `Microsoft.DocumentDB/databaseAccounts/mongoMIRoleAssignments` answers `501`.

## Why it is kept at 501

The type has a schema it could be served from, and is not: access control for a data plane; stored without being enforced, it would look like protection.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | GET | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/mongoMIRoleAssignments` | `MongoMIResources_ListMongoMIRoleAssignments` |
| `501` | DELETE | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/mongoMIRoleAssignments/{roleAssignmentId}` | `MongoMIResources_DeleteMongoMIRoleAssignment` |
| `501` | GET | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/mongoMIRoleAssignments/{roleAssignmentId}` | `MongoMIResources_GetMongoMIRoleAssignment` |
| `501` | PUT | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/mongoMIRoleAssignments/{roleAssignmentId}` | `MongoMIResources_CreateUpdateMongoMIRoleAssignment` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/cosmosdb/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/cosmosdb/CONTRACT.md`, section `Microsoft.DocumentDB/databaseAccounts/mongoMIRoleAssignments`, says what the specification requires of the type's name and body.
- Remove the type from `GENERIC_EXCEPTIONS` in `services/cosmosdb/src/lib.rs` once a handler serves it.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
