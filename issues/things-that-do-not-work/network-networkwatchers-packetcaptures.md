Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/networkWatchers/packetCaptures

# Implement Microsoft.Network/networkWatchers/packetCaptures

Every operation of `Microsoft.Network/networkWatchers/packetCaptures` answers `501`.

## Why it is kept at 501

The type has a schema it could be served from, and is not: starts a capture; storing the setting would report one that is not running.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | GET | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/packetCaptures` | `PacketCaptures_List` |
| `501` | DELETE | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/packetCaptures/{packetCaptureName}` | `PacketCaptures_Delete` |
| `501` | GET | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/packetCaptures/{packetCaptureName}` | `PacketCaptures_Get` |
| `501` | PUT | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/packetCaptures/{packetCaptureName}` | `PacketCaptures_Create` |
| `501` | POST | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/packetCaptures/{packetCaptureName}/queryStatus` | `PacketCaptures_GetStatus` |
| `501` | POST | `.../Microsoft.Network/networkWatchers/{networkWatcherName}/packetCaptures/{packetCaptureName}/stop` | `PacketCaptures_Stop` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/networkWatchers/packetCaptures`, says what the specification requires of the type's name and body.
- Remove the type from `GENERIC_EXCEPTIONS` in `services/network/src/lib.rs` once a handler serves it.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
