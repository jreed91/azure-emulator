Issue-Kind: resource-type
Issue-Service: services/eventhub
Issue-Key: Microsoft.EventHub/namespaces/authorizationRules

# Implement the remaining operations of Microsoft.EventHub/namespaces/authorizationRules

`Microsoft.EventHub/namespaces/authorizationRules` is served by a handler, but 1 of its 7 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.EventHub/namespaces/{namespaceName}/AuthorizationRules/{authorizationRuleName}` | `Namespaces_PostAuthorizationRule` |
| `ok` | GET | `.../Microsoft.EventHub/namespaces/{namespaceName}/authorizationRules` | `Namespaces_ListAuthorizationRules` |
| `ok` | DELETE | `.../Microsoft.EventHub/namespaces/{namespaceName}/authorizationRules/{authorizationRuleName}` | `Namespaces_DeleteAuthorizationRule` |
| `ok` | GET | `.../Microsoft.EventHub/namespaces/{namespaceName}/authorizationRules/{authorizationRuleName}` | `Namespaces_GetAuthorizationRule` |
| `ok` | PUT | `.../Microsoft.EventHub/namespaces/{namespaceName}/authorizationRules/{authorizationRuleName}` | `Namespaces_CreateOrUpdateAuthorizationRule` |
| `ok` | POST | `.../Microsoft.EventHub/namespaces/{namespaceName}/authorizationRules/{authorizationRuleName}/listKeys` | `Namespaces_ListKeys` |
| `ok` | POST | `.../Microsoft.EventHub/namespaces/{namespaceName}/authorizationRules/{authorizationRuleName}/regenerateKeys` | `Namespaces_RegenerateKeys` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/eventhub/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/eventhub/CONTRACT.md`, section `Microsoft.EventHub/namespaces/authorizationRules`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
