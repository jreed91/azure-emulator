Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/interconnectGroups

# Implement the remaining operations of Microsoft.Network/interconnectGroups

`Microsoft.Network/interconnectGroups` is served from its schema, but 1 of its 7 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Network/interconnectGroups/{interconnectGroupName}/nodeAvailability` | `InterconnectGroups_GetNodeAvailability` |
| `generic` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.Network/interconnectGroups` | `InterconnectGroups_ListAll` |
| `generic` | GET | `.../Microsoft.Network/interconnectGroups` | `InterconnectGroups_List` |
| `generic` | DELETE | `.../Microsoft.Network/interconnectGroups/{interconnectGroupName}` | `InterconnectGroups_Delete` |
| `generic` | GET | `.../Microsoft.Network/interconnectGroups/{interconnectGroupName}` | `InterconnectGroups_Get` |
| `generic` | PATCH | `.../Microsoft.Network/interconnectGroups/{interconnectGroupName}` | `InterconnectGroups_UpdateTags` |
| `generic` | PUT | `.../Microsoft.Network/interconnectGroups/{interconnectGroupName}` | `InterconnectGroups_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/interconnectGroups`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
