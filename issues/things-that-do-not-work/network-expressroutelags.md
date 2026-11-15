Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/expressRouteLags

# Implement the remaining operations of Microsoft.Network/expressRouteLags

`Microsoft.Network/expressRouteLags` is served from its schema, but 1 of its 7 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Network/expressRouteLags/{expressRouteLagName}/generateLoa` | `ExpressRouteLags_GenerateLoa` |
| `generic` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.Network/expressRouteLags` | `ExpressRouteLags_List` |
| `generic` | GET | `.../Microsoft.Network/expressRouteLags` | `ExpressRouteLags_ListByResourceGroup` |
| `generic` | DELETE | `.../Microsoft.Network/expressRouteLags/{expressRouteLagName}` | `ExpressRouteLags_Delete` |
| `generic` | GET | `.../Microsoft.Network/expressRouteLags/{expressRouteLagName}` | `ExpressRouteLags_Get` |
| `generic` | PATCH | `.../Microsoft.Network/expressRouteLags/{expressRouteLagName}` | `ExpressRouteLags_Update` |
| `generic` | PUT | `.../Microsoft.Network/expressRouteLags/{expressRouteLagName}` | `ExpressRouteLags_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/expressRouteLags`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
