Issue-Kind: data-plane-operation
Issue-Service: services/storage
Issue-Key: Azure Files/File_AcquireLease

# Implement Azure Files File_AcquireLease

`PUT /{shareName}/{directory}/{fileName}`, with `header x-ms-lease-action=acquire`, with `query comp=lease` answers `501` on the Azure Files data plane.

## Where to work

- The data plane's request router in `services/storage/src/`; operations sharing a path are told apart by the query and header values above.
- Write the behaviour from Azure's REST documentation for the operation, and a fidelity test in `tests/sdk/` or `tests/sdk-js/` that asserts it through the SDK.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- `azure-emulator coverage --operations` lists `File_AcquireLease` as `data ok`;
- the fidelity test passes, and README "What works today" and "Known gaps" say what changed;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
