# Security

Azure Emulator is for local development and tests. It is not a security
boundary: by default any client id and secret get a token, role assignments are
enforced only with `--enforce-rbac`, and its certificate authority is generated
on your machine. Do not expose it to untrusted networks, and do not put real
secrets or production data in it.

## Reporting a vulnerability

Report a vulnerability in the emulator privately, through a
[GitHub security advisory](https://github.com/jreed91/azure-emulator/security/advisories/new)
on this repository. Do not open a public issue.

Include the version (release tag, image tag or commit), how to reproduce it,
and what an attacker could do with it. Differences from Azure's own security
behaviour that only matter inside a test (for example, a check Azure makes that
the emulator does not) are ordinary issues: use the "Behavior differs from
Azure" form.
