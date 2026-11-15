Issue-Kind: provider-operation
Issue-Service: services/storage
Issue-Key: Microsoft.Storage/ContextCaches_CheckNameAvailability

# Implement Microsoft.Storage ContextCaches_CheckNameAvailability

This answers `501`:

- `POST /subscriptions/{subscriptionId}/providers/Microsoft.Storage/contextCacheCheckNameAvailability`

It is not an operation on one resource type: it is scoped to the provider, a location, a management group or the tenant, so the generic Resource Manager routing does not serve it.

## Where to work

- Register a handler for the operation's id with `registry.register_operation` in `services/storage/src/` (`services/keyvault/src/management.rs` has examples).
- If the answer is data about Azure itself that the specification does not contain (SKUs, regions, quotas, the operation catalogue), it stays `501`: say so in README "Not implemented, and why" instead.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists `ContextCaches_CheckNameAvailability` as `ok`, and a test reaches it;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
