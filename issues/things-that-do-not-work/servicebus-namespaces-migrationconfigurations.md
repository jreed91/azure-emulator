Issue-Kind: resource-type
Issue-Service: services/servicebus
Issue-Key: Microsoft.ServiceBus/namespaces/migrationConfigurations

# Implement the remaining operations of Microsoft.ServiceBus/namespaces/migrationConfigurations

`Microsoft.ServiceBus/namespaces/migrationConfigurations` is served by a handler, but 1 of its 6 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.ServiceBus/namespaces/{namespaceName}/migrationConfigurations/{configName}/upgrade` | `MigrationConfigs_CompleteMigration` |
| `ok` | GET | `.../Microsoft.ServiceBus/namespaces/{namespaceName}/migrationConfigurations` | `MigrationConfigs_List` |
| `ok` | DELETE | `.../Microsoft.ServiceBus/namespaces/{namespaceName}/migrationConfigurations/{configName}` | `MigrationConfigs_Delete` |
| `ok` | GET | `.../Microsoft.ServiceBus/namespaces/{namespaceName}/migrationConfigurations/{configName}` | `MigrationConfigs_Get` |
| `ok` | PUT | `.../Microsoft.ServiceBus/namespaces/{namespaceName}/migrationConfigurations/{configName}` | `MigrationConfigs_CreateAndStartMigration` |
| `ok` | POST | `.../Microsoft.ServiceBus/namespaces/{namespaceName}/migrationConfigurations/{configName}/revert` | `MigrationConfigs_Revert` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/servicebus/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/servicebus/CONTRACT.md`, section `Microsoft.ServiceBus/namespaces/migrationConfigurations`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
