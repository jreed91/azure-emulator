Issue-Kind: resource-type
Issue-Service: services/eventhub
Issue-Key: Microsoft.EventHub/clusters/upgradePreferences

# Implement the remaining operations of Microsoft.EventHub/clusters/upgradePreferences

`Microsoft.EventHub/clusters/upgradePreferences` is served from its schema, but 1 of its 3 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.EventHub/clusters/{clusterName}/upgradePreferences/default/upgradeNow` | `UpgradePreferencesOperations_UpgradeNow` |
| `generic` | GET | `.../Microsoft.EventHub/clusters/{clusterName}/upgradePreferences/default` | `UpgradePreferencesOperations_Get` |
| `generic` | PUT | `.../Microsoft.EventHub/clusters/{clusterName}/upgradePreferences/default` | `UpgradePreferencesOperations_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/eventhub/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/eventhub/CONTRACT.md`, section `Microsoft.EventHub/clusters/upgradePreferences`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
