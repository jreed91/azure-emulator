# Working one issue

The prompt given to an agent (or the checklist for a person) taking one file
from `issues/things-that-do-not-work/`. Replace `<ISSUE>` with the file's path.

---

Work the issue in `<ISSUE>` in this repository until it is done.

1. Read `AGENTS.md`, then the issue file. The issue names the crate, the
   operations and what done means; `AGENTS.md` has the rules and the commands.
2. Read how the crate already does similar things, and the type's section of
   the crate's `CONTRACT.md`, before writing anything. Where another provider
   already implements the same behaviour, reuse it rather than writing it again.
3. Write the behaviour from the specification and Azure's documentation. Where
   you do not know how Azure behaves, do not guess silently: mark the code
   `// Unverified against Azure: ...`. If the honest answer is that the
   emulator cannot do this (it is data about Azure, or needs something the
   emulator does not model), stop and say so instead of fabricating a result.
4. Add a fidelity test through an official SDK in `tests/sdk/` (or
   `tests/sdk-js/`) that asserts results, the state left behind and the errors
   for invalid calls, and that reaches every operation you implemented. Use
   `arm_request` only where no SDK method exists.
5. Run, and get passing: `cargo clippy --all-targets -- -D warnings`,
   `cargo test`, the SDK test file you touched, and
   `azure-emulator coverage --operations` showing the issue's operations as
   `ok`.
6. Update README "What works today" and "Known gaps" for what changed.
7. Run `python tools/issues/generate.py`; the issue's file moves to
   `issues/things-that-now-work/`. Do not edit files under `issues/` by hand.

Do not commit or push. Report: the files changed, what you implemented and from
which source, the commands you ran with their results, everything marked
unverified, and anything in the issue you did not do and why.
