Issue-Kind: resource-type
Issue-Service: services/servicebus
Issue-Key: Microsoft.ServiceBus/namespaces/messagingplan

# Implement Microsoft.ServiceBus/namespaces/messagingplan

Every operation of `Microsoft.ServiceBus/namespaces/messagingplan` answers `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | GET | `.../Microsoft.ServiceBus/namespaces/{namespaceName}/messagingplan` | `MessagingPlan_Get` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/servicebus/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/servicebus/CONTRACT.md`, section `Microsoft.ServiceBus/namespaces/messagingplan`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
