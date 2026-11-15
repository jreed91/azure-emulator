Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/bastionHosts

# Implement the remaining operations of Microsoft.Network/bastionHosts

`Microsoft.Network/bastionHosts` is served from its schema, but 6 of its 12 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Network/bastionHosts/{bastionHostName}/createShareableLinks` | `PutBastionShareableLink` |
| `501` | POST | `.../Microsoft.Network/bastionHosts/{bastionHostName}/deleteShareableLinks` | `DeleteBastionShareableLink` |
| `501` | POST | `.../Microsoft.Network/bastionHosts/{bastionHostName}/deleteShareableLinksByToken` | `DeleteBastionShareableLinkByToken` |
| `501` | POST | `.../Microsoft.Network/bastionHosts/{bastionHostName}/disconnectActiveSessions` | `DisconnectActiveSessions` |
| `501` | POST | `.../Microsoft.Network/bastionHosts/{bastionHostName}/getActiveSessions` | `GetActiveSessions` |
| `501` | POST | `.../Microsoft.Network/bastionHosts/{bastionHostName}/getShareableLinks` | `GetBastionShareableLink` |
| `generic` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.Network/bastionHosts` | `BastionHosts_List` |
| `generic` | GET | `.../Microsoft.Network/bastionHosts` | `BastionHosts_ListByResourceGroup` |
| `generic` | DELETE | `.../Microsoft.Network/bastionHosts/{bastionHostName}` | `BastionHosts_Delete` |
| `generic` | GET | `.../Microsoft.Network/bastionHosts/{bastionHostName}` | `BastionHosts_Get` |
| `generic` | PATCH | `.../Microsoft.Network/bastionHosts/{bastionHostName}` | `BastionHosts_Update` |
| `generic` | PUT | `.../Microsoft.Network/bastionHosts/{bastionHostName}` | `BastionHosts_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/bastionHosts`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
