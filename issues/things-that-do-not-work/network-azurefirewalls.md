Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/azureFirewalls

# Implement the remaining operations of Microsoft.Network/azureFirewalls

`Microsoft.Network/azureFirewalls` is served from its schema, but 3 of its 9 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `.../Microsoft.Network/azureFirewalls/{azureFirewallName}/learnedIPPrefixes` | `AzureFirewalls_ListLearnedPrefixes` |
| `501` | POST | `.../Microsoft.Network/azureFirewalls/{azureFirewallName}/packetCapture` | `AzureFirewalls_PacketCapture` |
| `501` | POST | `.../Microsoft.Network/azureFirewalls/{azureFirewallName}/packetCaptureOperation` | `AzureFirewalls_PacketCaptureOperation` |
| `generic` | GET | `/subscriptions/{subscriptionId}/providers/Microsoft.Network/azureFirewalls` | `AzureFirewalls_ListAll` |
| `generic` | GET | `.../Microsoft.Network/azureFirewalls` | `AzureFirewalls_List` |
| `generic` | DELETE | `.../Microsoft.Network/azureFirewalls/{azureFirewallName}` | `AzureFirewalls_Delete` |
| `generic` | GET | `.../Microsoft.Network/azureFirewalls/{azureFirewallName}` | `AzureFirewalls_Get` |
| `generic` | PATCH | `.../Microsoft.Network/azureFirewalls/{azureFirewallName}` | `AzureFirewalls_UpdateTags` |
| `generic` | PUT | `.../Microsoft.Network/azureFirewalls/{azureFirewallName}` | `AzureFirewalls_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/azureFirewalls`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
