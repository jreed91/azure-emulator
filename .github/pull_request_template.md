## What and why

<!-- What this changes for someone using the emulator, and the issue it addresses (Fixes #...). -->

## Checklist

See [AGENTS.md](../AGENTS.md) for the commands and rules.

- [ ] `cargo clippy --all-targets -- -D warnings` and `cargo test` pass
- [ ] The Python (`tests/sdk`) and JavaScript (`tests/sdk-js`) SDK suites pass, and Terraform and CLI if the change touches what they drive
- [ ] Every new SDK test file is listed in `tests/fidelity/services.json` (`python tests/fidelity/score.py --check`)
- [ ] Every newly implemented operation is reached by a Python, JavaScript, Terraform or CLI test (`tests/operations/check.py`)
- [ ] Known differences from Azure are `known_gap` tests, not weakened assertions
- [ ] Nothing fabricated, no panics in handlers, unimplemented paths answer `501`, returned endpoints come from `Config`
- [ ] Generated files (`src/scaffold.rs`, `CONTRACT.md`, `spec-examples.json`) were regenerated, not hand-edited; any `vendor/` change is recorded in `PATCHED.md`
- [ ] README "What works today" and "Known gaps" are updated if behaviour changed
- [ ] Commit subjects follow the release convention (`feat:` minor, `BREAKING CHANGE:` footer major, anything else patch)
