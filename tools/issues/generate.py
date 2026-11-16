#!/usr/bin/env python3
"""Writes one file per thing the emulator does not do yet, under ``issues/``.

The files are generated, never edited by hand, from what the emulator itself
reports and from the tests:

* every resource type with a Resource Manager operation that answers ``501``
  (``azure-emulator coverage --json``), with the reason where its crate keeps
  the type at ``501`` on purpose;
* every provider-level Resource Manager operation that answers ``501``;
* every data-plane operation that answers ``501``;
* every ``known_gap`` test: a difference from Azure the suite asserts.

``issues/things-that-do-not-work/`` holds the open ones. A file the generator
no longer writes (the operation works, the test passes) is moved to
``issues/things-that-now-work/``, so the history of what was fixed stays.

    cargo build -p azure-emulator
    python tools/issues/generate.py            # rewrite issues/
    python tools/issues/generate.py --check    # fail if issues/ is out of date

``AZURE_EMULATOR_BIN`` names a binary other than ``target/debug/azure-emulator``.
"""

import argparse
import collections
import json
import os
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
OPEN = ROOT / "issues" / "things-that-do-not-work"
DONE = ROOT / "issues" / "things-that-now-work"
INDEX = ROOT / "issues" / "README.md"
MANAGEMENT = "Resource Manager"
GROUP = "/subscriptions/{subscriptionId}/resourceGroups/{resourceGroupName}/providers/"


def coverage(binary):
    out = subprocess.run([binary, "coverage", "--json"], check=True, capture_output=True, text=True).stdout
    return json.loads(out)["operations"]


def slug(text):
    return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")


def services():
    """Crate directory by provider namespace and by data-plane name."""
    recorded = json.loads((ROOT / ".emulator" / "scaffolded-specs.json").read_text())
    found = {}
    for key, entry in recorded.items():
        found[entry.get("namespace") or entry.get("dataPlane")] = "services/" + key.split("-")[0]
    return found


def short(path):
    """A path without the subscription and resource group every one starts with."""
    return path.replace(GROUP, ".../") if path.startswith(GROUP) else path


def table(operations):
    lines = ["| Status | Method | Path | Operation |", "|---|---|---|---|"]
    for op in sorted(operations, key=lambda o: (o["status"] != "501", o["path"].lower(), o["method"])):
        selected = "".join(f" ({s})" for s in op.get("selectedBy") or [])
        lines.append(f"| `{op['status']}` | {op['method']} | `{short(op['path'])}`{selected} | `{op['id']}` |")
    return "\n".join(lines)


def header(kind, service, key):
    return f"Issue-Kind: {kind}\nIssue-Service: {service}\nIssue-Key: {key}\n"


RULES = (
    "Follow `AGENTS.md`: never fabricate a result, never panic in a handler, and add a "
    "Python, JavaScript, Terraform or CLI test that reaches each operation you implement."
)


def type_issue(provider, kind, operations, service):
    missing = [op for op in operations if op["status"] == "501"]
    working = [op for op in operations if op["status"] != "501"]
    reason = next((op["keptAt501"] for op in operations if op.get("keptAt501")), None)
    name = kind.split("/", 1)[1]
    if not working:
        title = f"Implement {kind}"
        summary = f"Every operation of `{kind}` answers `501`."
    else:
        served = "from its schema" if any(op["status"] == "generic" for op in working) else "by a handler"
        title = f"Implement the remaining operations of {kind}"
        summary = f"`{kind}` is served {served}, but {len(missing)} of its {len(operations)} operations answer `501`."
    body = [header("resource-type", service, kind), f"# {title}\n", summary, ""]
    if reason:
        body += ["## Why it is kept at 501\n", f"The type has a schema it could be served from, and is not: {reason}.", ""]
    body += ["## Operations\n", table(operations), ""]
    body += [
        "## Where to work\n",
        f"- Write or extend a `ResourceTypeHandler` for the type in `{service}/src/`, registered in the crate's "
        "`register` before the scaffold. An operation the generic routing cannot express is registered by its "
        "`operationId` with `registry.register_operation`.",
        f"- `{service}/CONTRACT.md`, section `{kind}`, says what the specification requires of the type's name and body.",
    ]
    if reason:
        body.append(f"- Remove the type from `GENERIC_EXCEPTIONS` in `{service}/src/lib.rs` once a handler serves it.")
    body += [f"- {RULES}", ""]
    body += [
        "## Done when\n",
        f"- `azure-emulator coverage --operations` lists the operations above as `ok`;",
        "- the operations check (`tests/operations/check.py`) passes;",
        "- README \"What works today\" and \"Known gaps\" say what changed;",
        "- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.",
        "",
    ]
    return f"{slug(provider.split('.')[-1])}-{slug(name)}.md", "\n".join(body)


def operation_issue(provider, operation_id, operations, service):
    paths = "\n".join(f"- `{op['method']} {op['path']}`" for op in sorted(operations, key=lambda o: (o["path"].lower(), o["method"])))
    body = [
        header("provider-operation", service, f"{provider}/{operation_id}"),
        f"# Implement {provider} {operation_id}\n",
        "This answers `501`:\n",
        paths,
        "",
        "It is not an operation on one resource type: it is scoped to the provider, a location, a management group or "
        "the tenant, so the generic Resource Manager routing does not serve it.",
        "",
        "## Where to work\n",
        f"- Register a handler for the operation's id with `registry.register_operation` in `{service}/src/` "
        "(`services/keyvault/src/management.rs` has examples).",
        "- If the answer is data about Azure itself that the specification does not contain (SKUs, regions, quotas, "
        "the operation catalogue), it stays `501`: say so in README \"Not implemented, and why\" instead.",
        f"- {RULES}",
        "",
        "## Done when\n",
        f"- `azure-emulator coverage --operations` lists `{operation_id}` as `ok`, and a test reaches it;",
        "- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.",
        "",
    ]
    return f"{slug(provider.split('.')[-1])}-operation-{slug(operation_id)}.md", "\n".join(body)


def data_issue(op, service):
    selected = "".join(f", with `{s}`" for s in op.get("selectedBy") or [])
    body = [
        header("data-plane-operation", service, f"{op['plane']}/{op['id']}"),
        f"# Implement {op['plane']} {op['id']}\n",
        f"`{op['method']} {op['path']}`{selected} answers `501` on the {op['plane']} data plane.",
        "",
        "## Where to work\n",
        f"- The data plane's request router in `{service}/src/`; operations sharing a path are told apart by the "
        "query and header values above.",
        "- Write the behaviour from Azure's REST documentation for the operation, and a fidelity test in `tests/sdk/` "
        "or `tests/sdk-js/` that asserts it through the SDK.",
        f"- {RULES}",
        "",
        "## Done when\n",
        f"- `azure-emulator coverage --operations` lists `{op['id']}` as `data ok`;",
        "- the fidelity test passes, and README \"What works today\" and \"Known gaps\" say what changed;",
        "- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.",
        "",
    ]
    return f"{slug(op['plane'])}-data-{slug(op['id'])}.md", "\n".join(body)


def known_gaps():
    """(test file relative to tests/, test name, reason) for every known_gap test."""
    found = []
    python = re.compile(r'@pytest\.mark\.known_gap\(\s*"((?:[^"\\]|\\.)*)"\s*\)\s*\n(?:@[^\n]*\n)*def (\w+)')
    for file in sorted((ROOT / "tests" / "sdk").glob("test_*.py")):
        for reason, name in python.findall(file.read_text()):
            found.append((f"sdk/{file.name}", name, reason))
    javascript = re.compile(r'test\(\s*"((?:[^"\\]|\\.)*)"\s*,\s*\{\s*todo:\s*"known gap: ((?:[^"\\]|\\.)*)"')
    for file in sorted((ROOT / "tests" / "sdk-js").glob("*.test.js")):
        for name, reason in javascript.findall(file.read_text()):
            found.append((f"sdk-js/{file.name}", name, reason))
    return found


def gap_issue(file, name, reason, scored):
    service = scored.get(file) or "the emulator"
    body = [
        header("known-gap", f"tests/{file}", f"{file}::{name}"),
        f"# {service}: {reason}\n",
        f"The test `{name}` in `tests/{file}` asserts how Azure behaves and fails against the emulator. It is marked "
        "as a known gap, so it runs as a strict expected failure and counts against the "
        f"{service} fidelity score.",
        "",
        "## Where to work\n",
        "- Read the test for the exact behaviour expected, then implement it in the service's crate.",
        "- Never weaken the assertion to make it pass.",
        f"- {RULES}",
        "",
        "## Done when\n",
        "- the test passes with its known-gap marker removed (the suite fails while a passing test still has it);",
        "- README \"Known gaps\" no longer lists the difference;",
        "- `python tools/issues/generate.py` moves this file to `issues/things-that-now-work/`.",
        "",
    ]
    return f"gap-{slug(pathlib.PurePath(file).stem)}-{slug(name)}.md", "\n".join(body)


def build(operations):
    """Every open issue as {file name: text}, and the index."""
    by_service = services()
    scored = {k: v for k, v in json.loads((ROOT / "tests" / "fidelity" / "services.json").read_text()).items() if isinstance(v, str)}
    issues = {}
    counts = collections.Counter()

    def add(kind, group, pair):
        name, text = pair
        if name in issues:
            raise SystemExit(f"two issues would be written to {name}")
        issues[name] = text
        counts[(kind, group)] += 1

    types = collections.defaultdict(list)
    provider_level = collections.defaultdict(list)
    for op in operations:
        if op["plane"] != MANAGEMENT:
            if op["status"] == "501":
                add("Data-plane operations", op["plane"], data_issue(op, by_service.get(op["plane"], "services")))
        elif op.get("resourceType"):
            types[(op["provider"], op["resourceType"])].append(op)
        elif op["status"] == "501":
            provider_level[(op["provider"], op["id"])].append(op)
    for (provider, kind), ops in sorted(types.items()):
        if any(op["status"] == "501" for op in ops):
            add("Resource types", provider, type_issue(provider, kind, ops, by_service.get(provider, "services")))
    for (provider, operation_id), ops in sorted(provider_level.items()):
        add("Provider-level operations", provider, operation_issue(provider, operation_id, ops, by_service.get(provider, "services")))
    for file, name, reason in known_gaps():
        add("Known gaps (failing fidelity tests)", scored.get(file) or "the emulator", gap_issue(file, name, reason, scored))

    index = [
        "# Issues\n",
        "What the emulator does not do yet, one file each. Generated by `tools/issues/generate.py` from",
        "`azure-emulator coverage --json` and the `known_gap` tests; do not edit these files by hand.\n",
        "- `things-that-do-not-work/`: open. Each file says what is missing, where to work and when it is done.",
        "- `things-that-now-work/`: files the generator stopped writing, because the operation is served or the",
        "  test passes.\n",
        "Each open file also has a GitHub issue, opened by `.github/workflows/issues.yml`",
        "(`tools/issues/sync_github.py`) and closed when the file moves.\n",
        "Resource types served from their schema alone (the `Generic` column of `coverage`) are not listed: they",
        "answer, with the limits README \"Known gaps\" describes.\n",
        f"## Open: {len(issues)}\n",
    ]
    for kind in ["Resource types", "Provider-level operations", "Data-plane operations", "Known gaps (failing fidelity tests)"]:
        groups = sorted((group, n) for (k, group), n in counts.items() if k == kind)
        if groups:
            index += [f"### {kind}: {sum(n for _, n in groups)}\n", "| | Open |", "|---|---:|"]
            index += [f"| {group} | {n} |" for group, n in groups]
            index.append("")
    return issues, "\n".join(index)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--check", action="store_true", help="fail if issues/ is not what would be written")
    parser.add_argument("--coverage", help="a file holding `azure-emulator coverage --json`, instead of running the binary")
    args = parser.parse_args()
    if args.coverage:
        operations = json.loads(pathlib.Path(args.coverage).read_text())["operations"]
    else:
        binary = os.environ.get("AZURE_EMULATOR_BIN") or str(ROOT / "target" / "debug" / "azure-emulator")
        operations = coverage(binary)
    issues, index = build(operations)
    existing = {file.name: file.read_text() for file in OPEN.glob("*.md")} if OPEN.exists() else {}

    if args.check:
        stale = sorted(set(existing) - set(issues))
        missing = sorted(set(issues) - set(existing))
        changed = sorted(name for name in set(issues) & set(existing) if issues[name] != existing[name])
        index_stale = not INDEX.exists() or INDEX.read_text() != index
        for label, names in (("no longer open", stale), ("not written yet", missing), ("out of date", changed)):
            for name in names:
                print(f"{label}: issues/things-that-do-not-work/{name}")
        if stale or missing or changed or index_stale:
            print("issues/ is out of date; run: python tools/issues/generate.py")
            return 1
        print(f"issues/ is up to date: {len(issues)} open")
        return 0

    OPEN.mkdir(parents=True, exist_ok=True)
    DONE.mkdir(parents=True, exist_ok=True)
    for name in sorted(set(existing) - set(issues)):
        (OPEN / name).rename(DONE / name)
        print(f"now works: {name}")
    for name, text in issues.items():
        if existing.get(name) != text:
            (OPEN / name).write_text(text)
        # An issue that is open again is no longer among the fixed ones.
        (DONE / name).unlink(missing_ok=True)
    INDEX.write_text(index)
    print(f"{len(issues)} open issue(s) in issues/things-that-do-not-work/")
    return 0


if __name__ == "__main__":
    sys.exit(main())
