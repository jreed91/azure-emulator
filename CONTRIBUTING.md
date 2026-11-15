# Contributing

Issues and pull requests are both welcome.

## Reporting a difference from Azure

Open an [issue](https://github.com/jreed91/azure-emulator/issues/new/choose) and
pick the form that fits:

- **Behavior differs from Azure**: an Azure API the emulator does not support
  yet, or one that answers differently from Azure (a different response or
  error, or a missing side effect). Include the request you sent, what the
  emulator returned, what Azure returns and how you know, and how to reproduce
  it with an SDK, the Azure CLI or Terraform.
- **Feature request**: a change to the emulator itself, such as how you
  install, start, configure or run it.

Before filing, check `azure-emulator coverage --operations`, the README's
"Known gaps" and the files under `issues/things-that-do-not-work/`: an
operation that answers `501` is known to be missing, and has a file there.

Remove secrets, keys, connection strings, tokens, subscription and tenant ids,
and personal data from anything you paste. Security problems go through
[SECURITY.md](SECURITY.md), not a public issue.

## Proposing a change

1. For anything larger than a small fix, open an issue first so the approach
   can be agreed.
2. Read [AGENTS.md](AGENTS.md): it lists which files to edit, the commands CI
   runs, and the rules every change follows (never fabricate a result, answer
   anything unimplemented with `501`, generated files are not edited by hand).
3. Back the change with tests: an SDK, Terraform or CLI test for each operation
   you implement, and a `known_gap` test for any difference from Azure you know
   about but are not fixing.
4. Open a pull request and fill in the checklist. Commit subjects drive the
   release version (see "Commits and releases" in AGENTS.md).

The best report is often a pull request that adds a `known_gap` test asserting
Azure's behaviour: it documents the difference and counts towards the
service's fidelity score.
