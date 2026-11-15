Issue-Kind: resource-type
Issue-Service: services/storage
Issue-Key: Microsoft.Storage/storageAccounts/connectors

# Implement the remaining operations of Microsoft.Storage/storageAccounts/connectors

`Microsoft.Storage/storageAccounts/connectors` is served from its schema, but 1 of its 6 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Storage/storageAccounts/{accountName}/connectors/{connectorName}/testExistingConnection` | `Connectors_TestExistingConnection` |
| `generic` | GET | `.../Microsoft.Storage/storageAccounts/{accountName}/connectors` | `Connectors_ListByStorageAccount` |
| `generic` | DELETE | `.../Microsoft.Storage/storageAccounts/{accountName}/connectors/{connectorName}` | `Connectors_Delete` |
| `generic` | GET | `.../Microsoft.Storage/storageAccounts/{accountName}/connectors/{connectorName}` | `Connectors_Get` |
| `generic` | PATCH | `.../Microsoft.Storage/storageAccounts/{accountName}/connectors/{connectorName}` | `Connectors_Update` |
| `generic` | PUT | `.../Microsoft.Storage/storageAccounts/{accountName}/connectors/{connectorName}` | `Connectors_Create` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/storage/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/storage/CONTRACT.md`, section `Microsoft.Storage/storageAccounts/connectors`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
