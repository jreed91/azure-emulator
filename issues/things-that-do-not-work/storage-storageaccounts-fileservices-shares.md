Issue-Kind: resource-type
Issue-Service: services/storage
Issue-Key: Microsoft.Storage/storageAccounts/fileServices/shares

# Implement the remaining operations of Microsoft.Storage/storageAccounts/fileServices/shares

`Microsoft.Storage/storageAccounts/fileServices/shares` is served by a handler, but 1 of its 7 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Storage/storageAccounts/{accountName}/fileServices/default/shares/{shareName}/restore` | `FileShares_Restore` |
| `ok` | GET | `.../Microsoft.Storage/storageAccounts/{accountName}/fileServices/default/shares` | `FileShares_List` |
| `ok` | DELETE | `.../Microsoft.Storage/storageAccounts/{accountName}/fileServices/default/shares/{shareName}` | `FileShares_Delete` |
| `ok` | GET | `.../Microsoft.Storage/storageAccounts/{accountName}/fileServices/default/shares/{shareName}` | `FileShares_Get` |
| `ok` | PATCH | `.../Microsoft.Storage/storageAccounts/{accountName}/fileServices/default/shares/{shareName}` | `FileShares_Update` |
| `ok` | PUT | `.../Microsoft.Storage/storageAccounts/{accountName}/fileServices/default/shares/{shareName}` | `FileShares_Create` |
| `ok` | POST | `.../Microsoft.Storage/storageAccounts/{accountName}/fileServices/default/shares/{shareName}/lease` | `FileShares_Lease` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/storage/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/storage/CONTRACT.md`, section `Microsoft.Storage/storageAccounts/fileServices/shares`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
