Issue-Kind: resource-type
Issue-Service: services/eventhub
Issue-Key: Microsoft.EventHub/namespaces

# Implement the remaining operations of Microsoft.EventHub/namespaces

`Microsoft.EventHub/namespaces` is served by a handler, but 2 of its 8 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.EventHub/namespaces/{namespaceName}/AuthorizationRules` | `Namespaces_ListPostAuthorizationRules` |
| `501` | POST | `.../Microsoft.EventHub/namespaces/{namespaceName}/failover` | `Namespaces_Failover` |
| `ok` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.EventHub/namespaces` | `Namespaces_List` |
| `ok` | GET | `.../Microsoft.EventHub/namespaces` | `Namespaces_ListByResourceGroup` |
| `ok` | DELETE | `.../Microsoft.EventHub/namespaces/{namespaceName}` | `Namespaces_Delete` |
| `ok` | GET | `.../Microsoft.EventHub/namespaces/{namespaceName}` | `Namespaces_Get` |
| `ok` | PATCH | `.../Microsoft.EventHub/namespaces/{namespaceName}` | `Namespaces_Update` |
| `ok` | PUT | `.../Microsoft.EventHub/namespaces/{namespaceName}` | `Namespaces_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/eventhub/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/eventhub/CONTRACT.md`, section `Microsoft.EventHub/namespaces`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
