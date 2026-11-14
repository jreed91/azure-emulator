#!/usr/bin/env python3
"""Turns the SDK suites' JUnit reports into per-service fidelity scores.

    score.py --pytest sdk.xml --node blob.test.js.xml ... [--markdown out.md] [--json out.json]
    score.py --check    every test file is in services.json, and nothing else is

A fidelity test passes when every assertion about Azure's behaviour held. A
`known_gap` test (a pytest strict xfail, or a node:test `todo`) asserts Azure
behaviour the emulator does not match yet, so it counts as failing. A node
report's file name, minus `.xml`, names the test file it came from.
Standard library only, so it runs before any dependency is installed.
"""

import argparse
import json
import pathlib
import sys
import xml.etree.ElementTree as ET

HERE = pathlib.Path(__file__).resolve().parent


def verdict(case):
    failed = case.find("failure") is not None or case.find("error") is not None
    skipped = case.find("skipped")
    if skipped is not None:
        kind = skipped.get("type", "")
        if kind == "pytest.xfail":
            return "known_gap"
        if kind == "todo":
            return "known_gap" if failed else "gap_closed"
        return "skipped"
    return "failed" if failed else "passed"


def pytest_cases(path):
    for case in ET.parse(path).iter("testcase"):
        # classname is the module's dotted path relative to tests/sdk.
        module = case.get("classname", "").split(".")
        name = case.get("name")
        yield f"sdk/{'/'.join(module)}.py", name, verdict(case)


def node_cases(path):
    source = f"sdk-js/{pathlib.Path(path).name.removesuffix('.xml')}"

    def walk(element, prefix):
        for child in element:
            if child.tag == "testsuite":
                yield from walk(child, f"{prefix}{child.get('name')} > ")
            elif child.tag == "testcase":
                yield source, prefix + child.get("name"), verdict(child)

    yield from walk(ET.parse(path).getroot(), "")


def score(cases, services):
    checks, unmapped = [], set()
    for source, name, result in cases:
        if source not in services:
            unmapped.add(source)
            continue
        if services[source] is None:
            continue
        checks.append({"check_id": f"{source}::{name}", "service": services[source], "verdict": result})
    if unmapped:
        sys.exit(f"error: not in {HERE / 'services.json'}: {', '.join(sorted(unmapped))}")
    return checks


def table(checks, header):
    by_service = {}
    for check in checks:
        by_service.setdefault(check["service"], []).append(check)
    lines = [header, "", "| Service | Passing | Known gaps | Other failures | Score |", "|---|---:|---:|---:|---:|"]
    total = passing = 0
    for service in sorted(by_service):
        rows = by_service[service]
        ok = sum(c["verdict"] in ("passed", "gap_closed") for c in rows)
        gaps = sum(c["verdict"] == "known_gap" for c in rows)
        total += len(rows)
        passing += ok
        lines.append(f"| {service} | {ok} of {len(rows)} | {gaps} | {len(rows) - ok - gaps} | {100 * ok / len(rows):.0f}% |")
    lines.append(f"| **All services** | **{passing} of {total}** | | | **{100 * passing / max(total, 1):.0f}%** |")
    failing = [c for c in checks if c["verdict"] not in ("passed", "gap_closed")]
    if failing:
        lines += ["", "<details><summary>Tests not passing</summary>", ""]
        lines += [f"- `{c['check_id']}` ({c['service']}, {c['verdict'].replace('_', ' ')})" for c in failing]
        lines += ["", "</details>"]
    closed = [c for c in checks if c["verdict"] == "gap_closed"]
    if closed:
        lines += ["", "Known gaps that now pass (drop their `todo`):", ""]
        lines += [f"- `{c['check_id']}`" for c in closed]
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--pytest", action="append", default=[], help="a pytest --junitxml report")
    parser.add_argument("--node", action="append", default=[], help="a node:test junit report named <test file>.xml")
    parser.add_argument("--services", default=HERE / "services.json")
    parser.add_argument("--header", default="## Fidelity scores")
    parser.add_argument("--markdown", help="write the table here (default: stdout)")
    parser.add_argument("--json", help="write every check and its verdict here, one per line")
    parser.add_argument("--check", action="store_true", help="only check services.json against the test files")
    args = parser.parse_args()

    services = {k: v for k, v in json.loads(pathlib.Path(args.services).read_text()).items() if not k.startswith("_")}
    if args.check:
        tests = HERE.parent
        files = {f"sdk/{p.name}" for p in (tests / "sdk").glob("test_*.py")}
        files |= {f"sdk-js/{p.name}" for p in (tests / "sdk-js").glob("*.test.js")}
        problems = [f"{f}: not in services.json" for f in sorted(files - services.keys())]
        problems += [f"{f}: in services.json, but no such test file" for f in sorted(services.keys() - files)]
        sys.exit("\n".join(problems) or None)
    cases = [c for p in args.pytest for c in pytest_cases(p)] + [c for p in args.node for c in node_cases(p)]
    checks = score(cases, services)
    if not checks:
        sys.exit("error: the reports hold no tests")

    markdown = table(checks, args.header)
    if args.markdown:
        pathlib.Path(args.markdown).write_text(markdown)
    else:
        sys.stdout.write(markdown)
    if args.json:
        with open(args.json, "w") as out:
            for check in sorted(checks, key=lambda c: c["check_id"]):
                out.write(json.dumps(check) + "\n")


if __name__ == "__main__":
    main()
