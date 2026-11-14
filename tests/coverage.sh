#!/usr/bin/env bash
# Line coverage of the emulator under every test suite: the Rust tests, and
# the Python and JavaScript SDK suites run against an instrumented binary.
#
#   tests/coverage.sh            per-file summary
#   tests/coverage.sh --html     also an HTML report in target/llvm-cov/html
#
# Needs cargo-llvm-cov (`cargo install cargo-llvm-cov`, and
# `rustup component add llvm-tools-preview`), the Python SDK suite's
# requirements ($PYTHON, default python3), and the JS suite's node_modules.
# The emulator writes its coverage when it exits on SIGTERM, which the
# suites send.
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo"
python="${PYTHON:-python3}"

eval "$(cargo llvm-cov show-env --sh)"
cargo llvm-cov clean --workspace
cargo build -p azure-emulator
export AZURE_EMULATOR_BIN="$repo/target/debug/azure-emulator"
cargo test --workspace -q
(cd tests/sdk && "$python" -m pytest -q -n auto -p no:cacheprovider)
(cd tests/sdk-js && npm test --silent)
cargo llvm-cov report --summary-only --ignore-filename-regex 'vendor/|tools/'
if [[ "${1:-}" == "--html" ]]; then
  cargo llvm-cov report --html --ignore-filename-regex 'vendor/|tools/'
fi
