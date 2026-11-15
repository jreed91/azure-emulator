Issue-Kind: resource-type
Issue-Service: services/network
Issue-Key: Microsoft.Network/loadBalancers/backendAddressPools

# Implement the remaining operations of Microsoft.Network/loadBalancers/backendAddressPools

`Microsoft.Network/loadBalancers/backendAddressPools` is served from its schema, but 1 of its 5 operations answer `501`.

## Operations

| Status | Method | Path | Operation |
|---|---|---|---|
| `501` | POST | `/subscriptions/{subscriptionId}/resourceGroups/{groupName}/providers/Microsoft.Network/loadBalancers/{loadBalancerName}/backendAddressPools/{backendPoolName}/queryInboundNatRulePortMapping` | `LoadBalancers_ListInboundNatRulePortMappings` |
| `generic` | GET | `.../Microsoft.Network/loadBalancers/{loadBalancerName}/backendAddressPools` | `LoadBalancerBackendAddressPools_List` |
| `generic` | DELETE | `.../Microsoft.Network/loadBalancers/{loadBalancerName}/backendAddressPools/{backendAddressPoolName}` | `LoadBalancerBackendAddressPools_Delete` |
| `generic` | GET | `.../Microsoft.Network/loadBalancers/{loadBalancerName}/backendAddressPools/{backendAddressPoolName}` | `LoadBalancerBackendAddressPools_Get` |
| `generic` | PUT | `.../Microsoft.Network/loadBalancers/{loadBalancerName}/backendAddressPools/{backendAddressPoolName}` | `LoadBalancerBackendAddressPools_CreateOrUpdate` |

## Where to work

- Write or extend a `ResourceTypeHandler` for the type in `services/network/src/`, registered in the crate's `register` before the scaffold. An operation the generic routing cannot express is registered by its `operationId` with `registry.register_operation`.
- `services/network/CONTRACT.md`, section `Microsoft.Network/loadBalancers/backendAddressPools`, says what the specification requires of the type's name and body.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists the operations above as `ok`;
- the operations check (`tests/operations/check.py`) passes;
- README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
