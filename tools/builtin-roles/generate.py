"""Regenerates core/emulator-core/src/arm/authorization/builtin-roles.json.

Azure's built-in role definitions (ids, names, permissions) come from the
published documentation, which is generated from Azure itself:
https://github.com/MicrosoftDocs/azure-docs/tree/main/articles/role-based-access-control/built-in-roles

Usage: python3 tools/builtin-roles/generate.py DIR
where DIR holds the category pages (`storage.md`, `general.md`, ...) downloaded
from that folder.
"""

import json
import pathlib
import re
import sys

OUT = pathlib.Path(__file__).resolve().parents[2] / "core/emulator-core/src/arm/authorization/builtin-roles.json"


def main(folder):
    roles = {}
    for page in sorted(pathlib.Path(folder).glob("*.md")):
        for block in re.findall(r"```json\n(.*?)\n```", page.read_text(), re.S):
            role = json.loads(block)
            if role.get("type") != "Microsoft.Authorization/roleDefinitions":
                continue
            roles[role["name"]] = {
                "name": role["name"],
                "roleName": role["roleName"],
                "description": role.get("description", ""),
                "permissions": role.get("permissions", []),
            }
    ordered = sorted(roles.values(), key=lambda r: r["roleName"].lower())
    OUT.write_text(json.dumps(ordered, separators=(",", ":"), sort_keys=True) + "\n")
    print(f"{len(ordered)} roles -> {OUT}")


if __name__ == "__main__":
    main(sys.argv[1])
