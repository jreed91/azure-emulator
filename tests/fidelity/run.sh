#!/usr/bin/env bash
# Runs the fidelity tests (the Python and JavaScript SDK suites) and scores
# them per service. Failing tests lower the score; they do not fail this script.
#
#   tests/fidelity/run.sh [output directory, default target/fidelity]
#
# Needs a built emulator (cargo build -p azure-emulator, or $AZURE_EMULATOR_BIN),
# tests/sdk/requirements.txt installed for $PYTHON (default python3), and
# `npm ci` run in tests/sdk-js. Writes:
#   reports/     the JUnit reports
#   scores.md    the per-service table published with each release
#   checks.jsonl every fidelity test, its service and its verdict
set -uo pipefail
repo="$(cd "$(dirname "$0")/../.." && pwd)"
out="$(mkdir -p "${1:-$repo/target/fidelity}" && cd "${1:-$repo/target/fidelity}" && pwd)"
python="${PYTHON:-python3}"
rm -rf "$out/reports" && mkdir -p "$out/reports/sdk-js"

(cd "$repo/tests/sdk" && "$python" -m pytest -q -n auto -p no:cacheprovider --junitxml="$out/reports/sdk.xml")

node_reports=()
for file in "$repo"/tests/sdk-js/*.test.js; do
  name="$(basename "$file")"
  (cd "$repo/tests/sdk-js" && node --require ./dns.cjs --test --test-concurrency=1 \
    --test-reporter=dot --test-reporter-destination=stdout \
    --test-reporter=junit --test-reporter-destination="$out/reports/sdk-js/$name.xml" "$name")
  node_reports+=(--node "$out/reports/sdk-js/$name.xml")
done

"$python" "$repo/tests/fidelity/score.py" --pytest "$out/reports/sdk.xml" "${node_reports[@]}" \
  --markdown "$out/scores.md" --json "$out/checks.jsonl" || exit $?
cat "$out/scores.md"
