Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/privateLinkServices/privateEndpointConnections

# Implement Microsoft.Network/privateLinkServices/privateEndpointConnections

Every operation of `Microsoft.Network/privateLinkServices/privateEndpointConnections` answers `501`.

## Why it is kept at 501

The type has a schema it could be served from, and is not: created by a private endpoint connecting to the service, not by a PUT.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | GET | `.../Microsoft.Network/privateLinkServices/{serviceName}/privateEndpointConnections` | `PrivateLinkServices_ListPrivateEndpointConnections` |
| `501` | DELETE | `.../Microsoft.Network/privateLinkServices/{serviceName}/privateEndpointConnections/{peConnectionName}` | `PrivateLinkServices_DeletePrivateEndpointConnection` |
| `501` | GET | `.../Microsoft.Network/privateLinkServices/{serviceName}/privateEndpointConnections/{peConnectionName}` | `PrivateLinkServices_GetPrivateEndpointConnection` |
| `501` | PUT | `.../Microsoft.Network/privateLinkServices/{serviceName}/privateEndpointConnections/{peConnectionName}` | `PrivateLinkServices_UpdatePrivateEndpointConnection` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/privateLinkServices/privateEndpointConnections`, says what the specification requires of the type's name and body.
- Remove the type from `GENERIC_EXCEPTIONS` in `services/network/src/lib.rs` once a handler serves it.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
