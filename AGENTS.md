# Working on azure-emulator

The working guide for anyone, human or agent, changing the emulator. `README.md`
is the user-facing document; read its "Repository layout" through "Testing"
sections before a first change.

## How it fits together

One binary (`azure-emulator`, crate `core/router`) serves every API on one HTTPS
port (default `10443`), plus an AMQP broker (`5672`, TLS `5671`). Requests are
split by the `Host` header in `core/router/src/lib.rs`:

- **Resource Manager** (`localhost`): one generic ARM layer in
  `core/emulator-core/src/arm.rs` handles tokens, `api-version`, the resource
  envelope, storage, listing, deletion and cascade. Each resource type plugs in a
  `ResourceTypeHandler` (validation, `properties`, actions), a schema-served
  generic type (`registry.register_generic`), or an operation by `operationId`
  (`registry.register_operation`). Every other type in a provider's
  specification is mounted from the generated scaffold and answers `501`.
- **Data planes** (`<account>.blob.core.<dns_suffix>`, `.queue.`, `.table.`,
  `.file.`, `<vault>.vault.<dns_suffix>`, `<account>.documents.<dns_suffix>`):
  plain functions from a request to a response, chosen by hostname.
- **Login** (Entra ID token service, IMDS) and **AMQP** (Service Bus and Event
  Hubs, `services/servicebus/src/broker/`).

```
core/emulator-core/   engine (state), Config, tokens, the ARM layer;
                      src/arm/deployments/ (templates), src/arm/authorization/ (RBAC)
core/router/          the binary, embedded facade, provisioning, persistence; tests
services/<name>/      one crate per provider: src/lib.rs has `register`;
                      src/scaffold.rs, CONTRACT.md, spec-examples.json are generated
tools/scaffold/       emulator-scaffold: OpenAPI specification -> service scaffold
tools/builtin-roles/  generate.py regenerates the built-in role definitions
vendor/fe2o3-amqp/    the AMQP library, patched (PATCHED.md)
tests/sdk/, tests/sdk-js/   fidelity tests through the Python and JavaScript SDKs
tests/terraform/, tests/cli/  azurerm apply/plan/destroy; Azure CLI smoke test
tests/fidelity/       per-service scores; services.json maps test files to services
tests/operations/     check.py: every implemented operation is reached by a test
tools/issues/         generate.py writes issues/: one file per thing not done yet
```

## Which files do I edit?

| Want to... | Edit |
|---|---|
| Implement a resource type | a `ResourceTypeHandler` in `services/<name>/src/`, registered in that crate's `register` before the scaffold (`services/managedidentity/src/lib.rs` is a small complete example) |
| Serve a configuration-only type from its schema | nothing: `register_generic_remaining` in the crate's `register` serves every type with a `PUT` schema that has no handler |
| Keep a type at `501` although it has a schema | the crate's `GENERIC_EXCEPTIONS`, with the reason |
| Implement a location-, tenant- or state-backed operation | `registry.register_operation` with its `operationId` (examples in `services/keyvault/src/management.rs`) |
| Change the shared ARM behaviour | `core/emulator-core/src/arm.rs` (and `src/arm/`) |
| Add or change a data plane | the service crate, plus its hostname dispatch in `core/router/src/lib.rs` |
| Add a provider | run `emulator-scaffold` (below); it adds the crate and registers it |
| Add a cross-service Rust test | `core/router/tests/all/<name>.rs`, declared with `#[path]` in `core/router/tests/all.rs` |
| Add an SDK test file | `tests/sdk/test_<name>.py` or `tests/sdk-js/<name>.test.js`, and an entry in `tests/fidelity/services.json` |
| Record a spec example that cannot pass | `core/router/spec-examples-known-failures.txt`, with the reason |
| Change user-visible behaviour | also README "What works today" and "Known gaps" |

Never edit by hand: `services/*/src/scaffold.rs`, `services/*/src/*_scaffold.rs`,
`services/*/CONTRACT.md`,
`services/*/spec-examples.json` and `.emulator/scaffolded-specs.json` (rewritten
by the scaffolder; the last records the specification commit each scaffold came
from). Read `CONTRACT.md`
for what a type's name and body must look like.

## Commands

```bash
cargo build -p azure-emulator                 # the binary, target/debug/azure-emulator
cargo run -p azure-emulator -- --help         # run it; options and AZURE_EMULATOR_* variables
cargo run -p azure-emulator -- coverage --operations   # 501 / ok / generic per operation
cargo run -p emulator-scaffold -- --namespace Microsoft.X --service x \
    --spec /path/to/azure-rest-api-specs/specification/x/resource-manager

cargo clippy --all-targets -- -D warnings     # CI fails on any warning
cargo test                                    # in-process tests through the full router

pip install -r tests/sdk/requirements.txt
(cd tests/sdk && python -m pytest -n auto)    # Python SDK suite (needs the built binary)
(cd tests/sdk-js && npm ci && npm test)       # JavaScript SDK suite
python tests/fidelity/score.py --check        # every test file is in services.json
python tools/issues/generate.py [--check]     # rewrite (or verify) issues/
tests/fidelity/run.sh                         # scores into target/fidelity/

tests/terraform/run.sh --hosts-line | sudo tee -a /etc/hosts && tests/terraform/run.sh
tests/cli/run.sh --hosts-line | sudo tee -a /etc/hosts && tests/cli/run.sh
tests/coverage.sh [--html]                    # line coverage (needs cargo-llvm-cov)
```

`AZURE_EMULATOR_BIN` makes the suites test another binary. The operations check
reads traces the suites write when `AZURE_EMULATOR_TRACE=<file>` is set:

```bash
export AZURE_EMULATOR_TRACE=$PWD/target/python.trace; (cd tests/sdk && python -m pytest -n auto)
export AZURE_EMULATOR_TRACE=$PWD/target/js.trace; (cd tests/sdk-js && npm test)
python tests/operations/check.py target/python.trace target/js.trace
```

CI also feeds it the Terraform and CLI traces, so an operation only those reach
may be reported locally; add their traces to compare like for like.

## Rules

- **Never fabricate a result.** If the answer is data about Azure that the
  emulator does not have (SKUs, regions, quotas), answer `501`. Leave read-only
  values out rather than inventing them. Mark an error code or message not
  checked against Azure with a `// Unverified against Azure: ...` comment.
- **Never panic in a handler.** No `panic!`, `unwrap()` or `expect()` on request
  data; return an `ArmError` or an error response.
- **Unimplemented means `501`.** Do not return an empty success for something
  not modelled; the scaffold does this for types nobody has implemented.
- **Endpoints come from `Config`.** Any URL or hostname returned to a client
  (`primaryEndpoints`, `vaultUri`, connection strings, `environment()`
  suffixes) is built from `Config` (`blob_endpoint`, `vault_uri`,
  `servicebus_endpoint`, `dns_suffix`, ...), never from `*.windows.net`,
  `*.azure.com` or another real Azure host, so clients stay local.
- **Generated files are regenerated, not edited** (see above).
- **`vendor/` is patched only as recorded.** Change `vendor/fe2o3-amqp/` only
  when necessary, and list each change, its files and the reason in
  `vendor/fe2o3-amqp/PATCHED.md`.
- **Rust test suites live in `core/router/tests/all/`** and are declared in
  `all.rs`, so they build as one test binary. Helpers go in `support.rs`.
- **Known differences from Azure are published as tests.** Write the test
  asserting Azure's behaviour and mark it `@pytest.mark.known_gap("why")` in
  Python or `{ todo: "known gap: why" }` in JavaScript. It runs as a strict
  expected failure; when the emulator catches up it fails, and the marker is
  removed. Never weaken an assertion to make a test pass.
- **Every new SDK test file is listed in `tests/fidelity/services.json`**
  (`null` for a file that tests the emulator itself, not a match with Azure);
  CI runs `score.py --check`.
- **Every implemented operation is reached by a test.** Moving an operation
  from `501` to implemented means adding a Python, JavaScript, Terraform or CLI
  test that calls it, or an entry with a reason in
  `tests/operations/allowlist.txt` (read by `check.py`; the file does not exist
  until there is a first entry).
- **`issues/` is generated.** It lists what answers `501` and what the
  `known_gap` tests assert. After a change that implements an operation, adds
  a provider, or adds or closes a known gap, run
  `python tools/issues/generate.py` (it needs the built binary) and commit the
  result; CI runs it with `--check`. To pick up work, take a file from
  `issues/things-that-do-not-work/`: it names the crate, the operations and
  what done means.
- **Tests are independent.** SDK tests create their own resource group and
  uniquely named resources; pytest workers run in parallel, each with its own
  emulator.

## Commits and releases

Every commit to `main` that passes CI is released by semantic-release
(`.releaserc.json`, `conventionalcommits` preset). The commit messages since the last
release pick the version:

- a `BREAKING CHANGE:` footer in the body, or a `!` after the type
  (`feat!:`, `fix(scope)!:`): major;
- a `feat:` (or `feat(scope):`) subject: minor;
- anything else: patch.

Write subjects in the imperative and say what changed for a
user of the emulator.
