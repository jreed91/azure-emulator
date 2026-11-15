Issue-Kind: known-gap
Issue-Service: tests/sdk/test_network.py
Issue-Key: sdk/test_network.py::test_interface_gets_a_private_address

# Virtual networks: addresses are not allocated: a dynamic private address is never assigned

The test `test_interface_gets_a_private_address` in `tests/sdk/test_network.py` asserts how Azure behaves and fails against the emulator. It is marked as a known gap, so it runs as a strict expected failure and counts against the Virtual networks fidelity score.

## Where to work

- Read the test for the exact behaviour expected, then implement it in the service's crate.
- Never weaken the assertion to make it pass.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- the test passes with its known-gap marker removed (the suite fails while a passing test still has it);
- README "Known gaps" no longer lists the difference;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
