Issue-Kind: resource-type
Issue-Service: services/servicebus
Issue-Key: Microsoft.ServiceBus/namespaces

# Implement the remaining operations of Microsoft.ServiceBus/namespaces

`Microsoft.ServiceBus/namespaces` is served by a handler, but 2 of its 9 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.ServiceBus/namespaces/{namespaceName}/failover` | `Namespaces_Failover` |
| `501` | POST | `.../Microsoft.ServiceBus/namespaces/{namespaceName}/migrate` | `Namespaces_Migrate` |
| `ok` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.ServiceBus/namespaces` | `Namespaces_List` |
| `ok` | GET | `.../Microsoft.ServiceBus/namespaces` | `Namespaces_ListByResourceGroup` |
| `ok` | DELETE | `.../Microsoft.ServiceBus/namespaces/{namespaceName}` | `Namespaces_Delete` |
| `ok` | GET | `.../Microsoft.ServiceBus/namespaces/{namespaceName}` | `Namespaces_Get` |
| `ok` | PATCH | `.../Microsoft.ServiceBus/namespaces/{namespaceName}` | `Namespaces_Update` |
| `ok` | PUT | `.../Microsoft.ServiceBus/namespaces/{namespaceName}` | `Namespaces_CreateOrUpdate` |
| `ok` | POST | `.../Microsoft.ServiceBus/namespaces/{namespaceName}/AuthorizationRules` | `Namespaces_ListPostAuthorizationRules` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/servicebus/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/servicebus/CONTRACT.md`, section `Microsoft.ServiceBus/namespaces`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
