Issue-Kind: resource-type
Issue-Service: services/eventhub
Issue-Key: Microsoft.EventHub/namespaces/eventhubs/fabricShortcuts

# Implement the remaining operations of Microsoft.EventHub/namespaces/eventhubs/fabricShortcuts

`Microsoft.EventHub/namespaces/eventhubs/fabricShortcuts` is served from its schema, but 2 of its 6 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.EventHub/namespaces/{namespaceName}/eventhubs/{eventHubName}/fabricShortcuts/{fabricShortcutName}/approve` | `FabricShortcuts_Approve` |
| `501` | POST | `.../Microsoft.EventHub/namespaces/{namespaceName}/eventhubs/{eventHubName}/fabricShortcuts/{fabricShortcutName}/reject` | `FabricShortcuts_Reject` |
| `generic` | GET | `.../Microsoft.EventHub/namespaces/{namespaceName}/eventhubs/{eventHubName}/fabricShortcuts` | `FabricShortcuts_ListByEventHub` |
| `generic` | DELETE | `.../Microsoft.EventHub/namespaces/{namespaceName}/eventhubs/{eventHubName}/fabricShortcuts/{fabricShortcutName}` | `FabricShortcuts_Delete` |
| `generic` | GET | `.../Microsoft.EventHub/namespaces/{namespaceName}/eventhubs/{eventHubName}/fabricShortcuts/{fabricShortcutName}` | `FabricShortcuts_Get` |
| `generic` | PUT | `.../Microsoft.EventHub/namespaces/{namespaceName}/eventhubs/{eventHubName}/fabricShortcuts/{fabricShortcutName}` | `FabricShortcuts_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/eventhub/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/eventhub/CONTRACT.md`, section `Microsoft.EventHub/namespaces/eventhubs/fabricShortcuts`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
