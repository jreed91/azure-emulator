Issue-Kind: known-gap
Issue-Service: tests/sdk/test_cosmos_gaps.py
Issue-Key: sdk/test_cosmos_gaps.py::test_entra_token_needs_a_data_role

# Cosmos DB: an Entra ID token is accepted without a data-plane role assignment

The test `test_entra_token_needs_a_data_role` in `tests/sdk/test_cosmos_gaps.py` asserts how Azure behaves and fails against the emulator. It is marked as a known gap, so it runs as a strict expected failure and counts against the Cosmos DB fidelity score.

## Where to work

- Read the test for the exact behaviour expected, then implement it in the service's crate.
- Never weaken the assertion to make it pass.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- the test passes with its known-gap marker removed (the suite fails while a passing test still has it);
- README "Known gaps" no longer lists the difference;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
