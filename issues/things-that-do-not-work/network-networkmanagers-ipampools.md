Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/networkManagers/ipamPools

# Implement the remaining operations of Microsoft.Network/networkManagers/ipamPools

`Microsoft.Network/networkManagers/ipamPools` is served from its schema, but 2 of its 7 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Network/networkManagers/{networkManagerName}/ipamPools/{poolName}/getPoolUsage` | `IpamPools_GetPoolUsage` |
| `501` | POST | `.../Microsoft.Network/networkManagers/{networkManagerName}/ipamPools/{poolName}/listAssociatedResources` | `IpamPools_ListAssociatedResources` |
| `generic` | GET | `.../Microsoft.Network/networkManagers/{networkManagerName}/ipamPools` | `IpamPools_List` |
| `generic` | DELETE | `.../Microsoft.Network/networkManagers/{networkManagerName}/ipamPools/{poolName}` | `IpamPools_Delete` |
| `generic` | GET | `.../Microsoft.Network/networkManagers/{networkManagerName}/ipamPools/{poolName}` | `IpamPools_Get` |
| `generic` | PATCH | `.../Microsoft.Network/networkManagers/{networkManagerName}/ipamPools/{poolName}` | `IpamPools_Update` |
| `generic` | PUT | `.../Microsoft.Network/networkManagers/{networkManagerName}/ipamPools/{poolName}` | `IpamPools_Create` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/networkManagers/ipamPools`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
