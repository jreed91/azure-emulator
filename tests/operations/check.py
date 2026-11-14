#!/usr/bin/env python3
"""Checks that every implemented Resource Manager operation is reached by a test.

The test suites (Python SDK, JavaScript SDK, Terraform, Azure CLI) run the
emulator with ``AZURE_EMULATOR_TRACE=<file>``, which appends one line per
request: ``<plane> <operation> <status>``. This script compares the union of
those traces with the operations ``azure-emulator coverage --operations``
reports as implemented (``ok``) or served from the schema (``generic``).

    python tests/operations/check.py TRACE [TRACE ...]

To produce the traces locally, run each suite with the variable set, e.g.

    AZURE_EMULATOR_TRACE=$PWD/target/python.trace python -m pytest -n auto  # in tests/sdk

An operation counts as covered when some request reached it and was not
answered ``501``. Operations listed in ``tests/operations/allowlist.txt``
(one ``Namespace/OperationId  # why`` per line) are reported but do not fail the check.
Exits 1 if any other implemented operation is not covered.
"""

import argparse
import os
import pathlib
import re
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
REPO = HERE.parent.parent


def implemented(binary, listing=None):
    if listing:
        out = pathlib.Path(listing).read_text()
    else:
        out = subprocess.run([binary, "coverage", "--operations"], check=True, capture_output=True, text=True).stdout
    found = {}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) >= 4 and parts[0] in ("ok", "generic") and parts[-1].startswith("("):
            path = parts[2]
            namespace = path.rsplit("/providers/", 1)[1].split("/", 1)[0]
            found[f"{namespace}/{parts[-1].strip('()')}"] = (parts[0], parts[1], path)
    return found


def shape(method, path):
    """Operations with the same method and path, up to parameter names, are
    one route: a request to it is attributed to either."""
    return method.upper(), re.sub(r"\{[^}]*\}", "{}", path).lower()


def covered(traces):
    seen = set()
    for trace in traces:
        for line in pathlib.Path(trace).read_text().splitlines():
            parts = line.split()
            if len(parts) == 3 and parts[0] == "arm" and parts[2] != "501":
                seen.add(parts[1])
    return seen


def allowlist():
    path = HERE / "allowlist.txt"
    allowed = {}
    if path.exists():
        for line in path.read_text().splitlines():
            entry, _, why = line.partition("#")
            if entry.strip():
                allowed[entry.strip()] = why.strip()
    return allowed


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("traces", nargs="+")
    parser.add_argument("--binary", default=os.environ.get("AZURE_EMULATOR_BIN", str(REPO / "target/debug/azure-emulator")))
    parser.add_argument("--operations", help="a saved `azure-emulator coverage --operations` listing, instead of running the binary")
    args = parser.parse_args()

    operations = implemented(args.binary, args.operations)
    seen = covered(args.traces)
    # A route's operations are covered together.
    reached_shapes = {shape(m, p) for op, (_, m, p) in operations.items() if op in seen}
    seen |= {op for op, (_, m, p) in operations.items() if shape(m, p) in reached_shapes}
    allowed = allowlist()
    missing = sorted(op for op in operations if op not in seen)
    failing = [op for op in missing if op not in allowed]
    stale = sorted(op for op in allowed if op in seen or op not in operations)

    total = len(operations)
    print(f"{total - len(missing)}/{total} implemented Resource Manager operations reached by a test")
    for op in missing:
        status, method, path = operations[op]
        note = f"  (allowed: {allowed[op]})" if op in allowed else ""
        print(f"  not reached: {op}  {method} {path}{note}")
    for op in stale:
        print(f"  allowlist entry no longer needed: {op}")
    if failing or stale:
        print(f"{len(failing)} operation(s) not reached and not allowlisted; {len(stale)} stale allowlist entr(ies)")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
