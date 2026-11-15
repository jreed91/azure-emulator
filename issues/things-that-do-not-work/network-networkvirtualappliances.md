Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/networkVirtualAppliances

# Implement the remaining operations of Microsoft.Network/networkVirtualAppliances

`Microsoft.Network/networkVirtualAppliances` is served from its schema, but 7 of its 13 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Network/networkVirtualAppliances/{networkVirtualApplianceName}/abortMigration` | `NetworkVirtualAppliances_AbortMigration` |
| `501` | POST | `.../Microsoft.Network/networkVirtualAppliances/{networkVirtualApplianceName}/commitMigration` | `NetworkVirtualAppliances_CommitMigration` |
| `501` | POST | `.../Microsoft.Network/networkVirtualAppliances/{networkVirtualApplianceName}/executeMigration` | `NetworkVirtualAppliances_ExecuteMigration` |
| `501` | POST | `.../Microsoft.Network/networkVirtualAppliances/{networkVirtualApplianceName}/getBootDiagnosticLogs` | `NetworkVirtualAppliances_GetBootDiagnosticLogs` |
| `501` | POST | `.../Microsoft.Network/networkVirtualAppliances/{networkVirtualApplianceName}/prepareMigration` | `NetworkVirtualAppliances_PrepareMigration` |
| `501` | POST | `.../Microsoft.Network/networkVirtualAppliances/{networkVirtualApplianceName}/reimage` | `NetworkVirtualAppliances_Reimage` |
| `501` | POST | `.../Microsoft.Network/networkVirtualAppliances/{networkVirtualApplianceName}/restart` | `NetworkVirtualAppliances_Restart` |
| `generic` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.Network/networkVirtualAppliances` | `NetworkVirtualAppliances_List` |
| `generic` | GET | `.../Microsoft.Network/networkVirtualAppliances` | `NetworkVirtualAppliances_ListByResourceGroup` |
| `generic` | DELETE | `.../Microsoft.Network/networkVirtualAppliances/{networkVirtualApplianceName}` | `NetworkVirtualAppliances_Delete` |
| `generic` | GET | `.../Microsoft.Network/networkVirtualAppliances/{networkVirtualApplianceName}` | `NetworkVirtualAppliances_Get` |
| `generic` | PATCH | `.../Microsoft.Network/networkVirtualAppliances/{networkVirtualApplianceName}` | `NetworkVirtualAppliances_UpdateTags` |
| `generic` | PUT | `.../Microsoft.Network/networkVirtualAppliances/{networkVirtualApplianceName}` | `NetworkVirtualAppliances_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/networkVirtualAppliances`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
