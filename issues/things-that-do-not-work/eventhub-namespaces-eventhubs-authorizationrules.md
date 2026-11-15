Issue-Kind: resource-type
Issue-Service: services/eventhub
Issue-Key: Microsoft.EventHub/namespaces/eventhubs/authorizationRules

# Implement the remaining operations of Microsoft.EventHub/namespaces/eventhubs/authorizationRules

`Microsoft.EventHub/namespaces/eventhubs/authorizationRules` is served by a handler, but 1 of its 7 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.EventHub/namespaces/{namespaceName}/eventhubs/{eventHubName}/authorizationRules/{authorizationRuleName}` | `EventHubs_PostAuthorizationRule` |
| `ok` | GET | `.../Microsoft.EventHub/namespaces/{namespaceName}/eventhubs/{eventHubName}/authorizationRules` | `EventHubs_ListAuthorizationRules` |
| `ok` | DELETE | `.../Microsoft.EventHub/namespaces/{namespaceName}/eventhubs/{eventHubName}/authorizationRules/{authorizationRuleName}` | `EventHubs_DeleteAuthorizationRule` |
| `ok` | GET | `.../Microsoft.EventHub/namespaces/{namespaceName}/eventhubs/{eventHubName}/authorizationRules/{authorizationRuleName}` | `EventHubs_GetAuthorizationRule` |
| `ok` | PUT | `.../Microsoft.EventHub/namespaces/{namespaceName}/eventhubs/{eventHubName}/authorizationRules/{authorizationRuleName}` | `EventHubs_CreateOrUpdateAuthorizationRule` |
| `ok` | POST | `.../Microsoft.EventHub/namespaces/{namespaceName}/eventhubs/{eventHubName}/authorizationRules/{authorizationRuleName}/listKeys` | `EventHubs_ListKeys` |
| `ok` | POST | `.../Microsoft.EventHub/namespaces/{namespaceName}/eventhubs/{eventHubName}/authorizationRules/{authorizationRuleName}/regenerateKeys` | `EventHubs_RegenerateKeys` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/eventhub/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/eventhub/CONTRACT.md`, section `Microsoft.EventHub/namespaces/eventhubs/authorizationRules`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
