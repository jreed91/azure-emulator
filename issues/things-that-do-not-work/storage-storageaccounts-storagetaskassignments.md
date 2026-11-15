Issue-Kind: resource-type
Issue-Service: services/storage
Issue-Key: Microsoft.Storage/storageAccounts/storageTaskAssignments

# Implement Microsoft.Storage/storageAccounts/storageTaskAssignments

Every operation of `Microsoft.Storage/storageAccounts/storageTaskAssignments` answers `501`.

## Why it is kept at 501

The type has a schema it could be served from, and is not: runs tasks of Microsoft.StorageActions, which the emulator does not model.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | GET | `.../Microsoft.Storage/storageAccounts/{accountName}/storageTaskAssignments` | `StorageTaskAssignments_List` |
| `501` | DELETE | `.../Microsoft.Storage/storageAccounts/{accountName}/storageTaskAssignments/{storageTaskAssignmentName}` | `StorageTaskAssignments_Delete` |
| `501` | GET | `.../Microsoft.Storage/storageAccounts/{accountName}/storageTaskAssignments/{storageTaskAssignmentName}` | `StorageTaskAssignments_Get` |
| `501` | PATCH | `.../Microsoft.Storage/storageAccounts/{accountName}/storageTaskAssignments/{storageTaskAssignmentName}` | `StorageTaskAssignments_Update` |
| `501` | PUT | `.../Microsoft.Storage/storageAccounts/{accountName}/storageTaskAssignments/{storageTaskAssignmentName}` | `StorageTaskAssignments_Create` |
| `501` | POST | `.../Microsoft.Storage/storageAccounts/{accountName}/storageTaskAssignments/{storageTaskAssignmentName}/stopAssignment` | `StorageTaskAssignments_StopAssignment` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/storage/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/storage/CONTRACT.md`, section `Microsoft.Storage/storageAccounts/storageTaskAssignments`, says what the specification requires of the type's name and body.
- Remove the type from `GENERIC_EXCEPTIONS` in `services/storage/src/lib.rs` once a handler serves it.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
