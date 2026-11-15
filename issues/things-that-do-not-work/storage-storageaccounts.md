Issue-Kind: resource-type
Issue-Service: services/storage
Issue-Key: Microsoft.Storage/storageAccounts

# Implement the remaining operations of Microsoft.Storage/storageAccounts

`Microsoft.Storage/storageAccounts` is served by a handler, but 6 of its 17 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Storage/storageAccounts/{accountName}/aborthnsonmigration` | `StorageAccounts_AbortHierarchicalNamespaceMigration` |
| `501` | POST | `.../Microsoft.Storage/storageAccounts/{accountName}/failover` | `StorageAccounts_Failover` |
| `501` | POST | `.../Microsoft.Storage/storageAccounts/{accountName}/hnsonmigration` | `StorageAccounts_HierarchicalNamespaceMigration` |
| `501` | POST | `.../Microsoft.Storage/storageAccounts/{accountName}/restoreBlobRanges` | `StorageAccounts_RestoreBlobRanges` |
| `501` | POST | `.../Microsoft.Storage/storageAccounts/{accountName}/startAccountMigration` | `StorageAccounts_CustomerInitiatedMigration` |
| `501` | POST | `.../Microsoft.Storage/storageAccounts/{accountName}/testBlobAccessPointConfigurationProposedConnection` | `BlobAccessPointConnectionTests_TestProposedConnection` |
| `ok` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.Storage/storageAccounts` | `StorageAccounts_List` |
| `ok` | GET | `.../Microsoft.Storage/storageAccounts` | `StorageAccounts_ListByResourceGroup` |
| `ok` | DELETE | `.../Microsoft.Storage/storageAccounts/{accountName}` | `StorageAccounts_Delete` |
| `ok` | GET | `.../Microsoft.Storage/storageAccounts/{accountName}` | `StorageAccounts_GetProperties` |
| `ok` | PATCH | `.../Microsoft.Storage/storageAccounts/{accountName}` | `StorageAccounts_Update` |
| `ok` | PUT | `.../Microsoft.Storage/storageAccounts/{accountName}` | `StorageAccounts_Create` |
| `ok` | POST | `.../Microsoft.Storage/storageAccounts/{accountName}/listAccountSas` | `StorageAccounts_ListAccountSAS` |
| `ok` | POST | `.../Microsoft.Storage/storageAccounts/{accountName}/listKeys` | `StorageAccounts_ListKeys` |
| `ok` | POST | `.../Microsoft.Storage/storageAccounts/{accountName}/listServiceSas` | `StorageAccounts_ListServiceSAS` |
| `ok` | POST | `.../Microsoft.Storage/storageAccounts/{accountName}/regenerateKey` | `StorageAccounts_RegenerateKey` |
| `ok` | POST | `.../Microsoft.Storage/storageAccounts/{accountName}/revokeUserDelegationKeys` | `StorageAccounts_RevokeUserDelegationKeys` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/storage/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/storage/CONTRACT.md`, section `Microsoft.Storage/storageAccounts`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
