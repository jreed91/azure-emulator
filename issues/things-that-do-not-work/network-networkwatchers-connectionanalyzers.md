Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/networkWatchers/connectionAnalyzers

# Implement the remaining operations of Microsoft.Network/networkWatchers/connectionAnalyzers

`Microsoft.Network/networkWatchers/connectionAnalyzers` is served from its schema, but 1 of its 6 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/connectionAnalyzers/{connectionAnalyzerName}/query` | `NetworkWatchers_ConnectionAnalyzersQuery` |
| `generic` | GET | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/connectionAnalyzers` | `NetworkWatchers_ConnectionAnalyzersList` |
| `generic` | DELETE | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/connectionAnalyzers/{connectionAnalyzerName}` | `NetworkWatchers_ConnectionAnalyzersDelete` |
| `generic` | GET | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/connectionAnalyzers/{connectionAnalyzerName}` | `NetworkWatchers_ConnectionAnalyzersGet` |
| `generic` | PATCH | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/connectionAnalyzers/{connectionAnalyzerName}` | `NetworkWatchers_ConnectionAnalyzersUpdateTags` |
| `generic` | PUT | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/connectionAnalyzers/{connectionAnalyzerName}` | `NetworkWatchers_ConnectionAnalyzersCreate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/networkWatchers/connectionAnalyzers`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
