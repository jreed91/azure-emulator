Issue-Kind: resource-type
Issue-Service: services/storage
Issue-Key: Microsoft.Storage/storageAccounts/blobServices/containers

# Implement the remaining operations of Microsoft.Storage/storageAccounts/blobServices/containers

`Microsoft.Storage/storageAccounts/blobServices/containers` is served by a handler, but 1 of its 9 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Storage/storageAccounts/{accountName}/blobServices/default/containers/{containerName}/migrate` | `BlobContainers_ObjectLevelWorm` |
| `ok` | GET | `.../Microsoft.Storage/storageAccounts/{accountName}/blobServices/default/containers` | `BlobContainers_List` |
| `ok` | DELETE | `.../Microsoft.Storage/storageAccounts/{accountName}/blobServices/default/containers/{containerName}` | `BlobContainers_Delete` |
| `ok` | GET | `.../Microsoft.Storage/storageAccounts/{accountName}/blobServices/default/containers/{containerName}` | `BlobContainers_Get` |
| `ok` | PATCH | `.../Microsoft.Storage/storageAccounts/{accountName}/blobServices/default/containers/{containerName}` | `BlobContainers_Update` |
| `ok` | PUT | `.../Microsoft.Storage/storageAccounts/{accountName}/blobServices/default/containers/{containerName}` | `BlobContainers_Create` |
| `ok` | POST | `.../Microsoft.Storage/storageAccounts/{accountName}/blobServices/default/containers/{containerName}/clearLegalHold` | `BlobContainers_ClearLegalHold` |
| `ok` | POST | `.../Microsoft.Storage/storageAccounts/{accountName}/blobServices/default/containers/{containerName}/lease` | `BlobContainers_Lease` |
| `ok` | POST | `.../Microsoft.Storage/storageAccounts/{accountName}/blobServices/default/containers/{containerName}/setLegalHold` | `BlobContainers_SetLegalHold` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/storage/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/storage/CONTRACT.md`, section `Microsoft.Storage/storageAccounts/blobServices/containers`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
