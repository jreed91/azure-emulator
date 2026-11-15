Issue-Kind: resource-type
Issue-Service: services/cosmosdb
Issue-Key: Microsoft.DocumentDB/databaseAccounts/notebookWorkspaces

# Implement the remaining operations of Microsoft.DocumentDB/databaseAccounts/notebookWorkspaces

`Microsoft.DocumentDB/databaseAccounts/notebookWorkspaces` is served from its schema, but 3 of its 7 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/notebookWorkspaces/{notebookWorkspaceName}/listConnectionInfo` | `NotebookWorkspaces_ListConnectionInfo` |
| `501` | POST | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/notebookWorkspaces/{notebookWorkspaceName}/regenerateAuthToken` | `NotebookWorkspaces_RegenerateAuthToken` |
| `501` | POST | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/notebookWorkspaces/{notebookWorkspaceName}/start` | `NotebookWorkspaces_Start` |
| `generic` | GET | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/notebookWorkspaces` | `NotebookWorkspaces_ListByDatabaseAccount` |
| `generic` | DELETE | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/notebookWorkspaces/{notebookWorkspaceName}` | `NotebookWorkspaces_Delete` |
| `generic` | GET | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/notebookWorkspaces/{notebookWorkspaceName}` | `NotebookWorkspaces_Get` |
| `generic` | PUT | `.../Microsoft.DocumentDB/databaseAccounts/{accountName}/notebookWorkspaces/{notebookWorkspaceName}` | `NotebookWorkspaces_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/cosmosdb/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/cosmosdb/CONTRACT.md`, section `Microsoft.DocumentDB/databaseAccounts/notebookWorkspaces`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
