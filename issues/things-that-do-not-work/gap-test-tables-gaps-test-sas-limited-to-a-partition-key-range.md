Issue-Kind: known-gap
Issue-Service: tests/sdk/test_tables_gaps.py
Issue-Key: sdk/test_tables_gaps.py::test_sas_limited_to_a_partition_key_range

# Table Storage: a table SAS scoped to a partition key range is verified but not narrowed to the range

The test `test_sas_limited_to_a_partition_key_range` in `tests/sdk/test_tables_gaps.py` asserts how Azure behaves and fails against the emulator. It is marked as a known gap, so it runs as a strict expected failure and counts against the Table Storage fidelity score.

## Where to work

- Read the test for the exact behaviour expected, then implement it in the service's crate.
- Never weaken the assertion to make it pass.
- Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a Python, JavaScript, Terraform or CLI test that reaches each operation you implement.

## Done when

- the test passes with its known-gap marker removed (the suite fails while a passing test still has it);
- README "Known gaps" no longer lists the difference;
- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.
