Issue-Kind: resource-type
Issue-Service: services/eventhub
Issue-Key: Microsoft.EventHub/namespaces/disasterRecoveryConfigs

# Implement Microsoft.EventHub/namespaces/disasterRecoveryConfigs

Every operation of `Microsoft.EventHub/namespaces/disasterRecoveryConfigs` answers `501`.

## Why it is kept at 501

The type has a schema it could be served from, and is not: pairs two namespaces; storing the setting would report a pairing that does not exist.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | GET | `.../Microsoft.EventHub/namespaces/{namespaceName}/disasterRecoveryConfigs` | `DisasterRecoveryConfigs_List` |
| `501` | POST | `.../Microsoft.EventHub/namespaces/{namespaceName}/disasterRecoveryConfigs/checkNameAvailability` | `DisasterRecoveryConfigs_CheckNameAvailability` |
| `501` | DELETE | `.../Microsoft.EventHub/namespaces/{namespaceName}/disasterRecoveryConfigs/{alias}` | `DisasterRecoveryConfigs_Delete` |
| `501` | GET | `.../Microsoft.EventHub/namespaces/{namespaceName}/disasterRecoveryConfigs/{alias}` | `DisasterRecoveryConfigs_Get` |
| `501` | PUT | `.../Microsoft.EventHub/namespaces/{namespaceName}/disasterRecoveryConfigs/{alias}` | `DisasterRecoveryConfigs_CreateOrUpdate` |
| `501` | POST | `.../Microsoft.EventHub/namespaces/{namespaceName}/disasterRecoveryConfigs/{alias}/breakPairing` | `DisasterRecoveryConfigs_BreakPairing` |
| `501` | POST | `.../Microsoft.EventHub/namespaces/{namespaceName}/disasterRecoveryConfigs/{alias}/failover` | `DisasterRecoveryConfigs_FailOver` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/eventhub/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/eventhub/CONTRACT.md`, section `Microsoft.EventHub/namespaces/disasterRecoveryConfigs`, says what the specification requires of the type's name and body.
- Remove the type from `GENERIC_EXCEPTIONS` in `services/eventhub/src/lib.rs` once a handler serves it.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
