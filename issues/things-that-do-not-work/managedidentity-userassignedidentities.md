Issue-Kind: resource-type
Issue-Service: services/managedidentity
Issue-Key: Microsoft.ManagedIdentity/userAssignedIdentities

# Implement the remaining operations of Microsoft.ManagedIdentity/userAssignedIdentities

`Microsoft.ManagedIdentity/userAssignedIdentities` is served by a handler, but 1 of its 8 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.ManagedIdentity/userAssignedIdentities/{revokeTokenResourceName}/revokeTokens` | `UserAssignedIdentities_RevokeTokens` |
| `ok` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.ManagedIdentity/userAssignedIdentities` | `UserAssignedIdentities_ListBySubscription` |
| `ok` | GET | `.../Microsoft.ManagedIdentity/userAssignedIdentities` | `UserAssignedIdentities_ListByResourceGroup` |
| `ok` | DELETE | `.../Microsoft.ManagedIdentity/userAssignedIdentities/{resourceName}` | `UserAssignedIdentities_Delete` |
| `ok` | GET | `.../Microsoft.ManagedIdentity/userAssignedIdentities/{resourceName}` | `UserAssignedIdentities_Get` |
| `ok` | PATCH | `.../Microsoft.ManagedIdentity/userAssignedIdentities/{resourceName}` | `UserAssignedIdentities_Update` |
| `ok` | PUT | `.../Microsoft.ManagedIdentity/userAssignedIdentities/{resourceName}` | `UserAssignedIdentities_CreateOrUpdate` |
| `ok` | POST | `.../Microsoft.ManagedIdentity/userAssignedIdentities/{resourceName}/listAssociatedResources` | `UserAssignedIdentities_ListAssociatedResources` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/managedidentity/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/managedidentity/CONTRACT.md`, section `Microsoft.ManagedIdentity/userAssignedIdentities`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
