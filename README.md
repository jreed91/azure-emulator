# Azure Emulator

A local emulator of the Azure APIs, for building and testing without an Azure
subscription. Your SDK calls go to a program on your machine and work against
in-memory resources.

It is modelled on [aws/aws-emulator](https://github.com/aws/aws-emulator): one
binary, one port, one crate per service, all state in memory, and an honest `501`
for anything not implemented yet.

**This is an early slice, not a complete emulator.** Do not use it for production
workloads or to test security.

## What works today

| Area | Implemented |
|---|---|
| Entra ID (login) | OpenID Connect discovery, instance discovery, signing keys, client-credentials tokens (v1 and v2 endpoints), managed-identity (IMDS) tokens |
| Resource Manager | Bearer-token verification, `api-version` checks, subscriptions, tenants, resource groups (create, get, exists, tag, list, delete with cascade), providers, generic resource listing, cloud metadata document |
| `Microsoft.Storage` | Storage accounts: create, get, update, delete, list, `listKeys`, `regenerateKey`, `checkNameAvailability` |
| Blob service | Containers: create, delete, properties, list. Block blobs: upload (single request), download with ranges, properties, metadata, delete, list with prefix, delimiter and paging. Authorization by Entra token or verified Shared Key |
| `Microsoft.KeyVault` | Vaults: create, get, update, delete, list, `checkNameAvailability` |
| Key Vault secrets | Set, get (latest or by version), update attributes, list, list versions, soft delete, get and list deleted, recover, purge |

`tests/sdk/python_smoke.py` exercises all of the above through the official Azure
SDKs for Python and passes. No other SDK, the Azure CLI or Terraform has been
tried yet.

### Known gaps

- Every other Azure service and resource type.
- Role assignments and Key Vault access policies are not enforced: any valid
  token for the right audience is allowed everything.
- There is no application registry: the token service accepts any client id that
  presents a credential.
- `api-version` is checked for shape only; every version gets the same behaviour.
- Blob: block lists (large uploads), page and append blobs, leases, snapshots,
  shared access signatures, copy, and conditional headers other than
  `If-None-Match: *`.
- Key Vault: keys, certificates, vault-level soft delete, and listing pages.
- Some error codes and messages are best-effort and have not been compared with
  real Azure; the code marks the ones known to be unverified.
- State is lost when the process exits.

## Quick start

```bash
cargo run -p azure-emulator
```

The emulator serves HTTPS on `127.0.0.1:10443` and prints its endpoints. Options:
`--port`, `--bind`, `--host`, `--dns-suffix`, `--http`; run with `--help` for
details.

### Trust the certificate

Azure SDKs refuse to send tokens over plain HTTP, so the emulator serves HTTPS
with a certificate issued by a local certificate authority. The CA is created on
first run at `~/.azure-emulator/ca.pem` (override the directory with
`AZURE_EMULATOR_HOME`) and reused afterwards, so you trust it once. For Python:

```bash
export REQUESTS_CA_BUNDLE=~/.azure-emulator/ca.pem
```

### Hostnames

Azure has no single endpoint override. Each storage account and vault has its own
hostname, and the emulator routes on the `Host` header:

| Service | Address |
|---|---|
| Resource Manager and login | `https://localhost:10443` |
| Blob | `https://<account>.blob.core.azure.localhost:10443` |
| Key Vault | `https://<vault>.vault.azure.localhost:10443` |

`*.localhost` names resolve to the loopback address in browsers, curl and on
systems using systemd-resolved, but not everywhere. If a name does not resolve,
add it to your hosts file:

```
127.0.0.1 myaccount.blob.core.azure.localhost my-vault.vault.azure.localhost
```

### Point an SDK at it

The default subscription is `00000000-0000-0000-0000-000000000000` and the
tenant is `11111111-1111-1111-1111-111111111111`. Any client id and secret work.

```python
from azure.identity import ClientSecretCredential
from azure.mgmt.resource.resources import ResourceManagementClient
from azure.storage.blob import BlobServiceClient

BASE = "https://localhost:10443"
credential = ClientSecretCredential(
    "11111111-1111-1111-1111-111111111111", "my-app", "any-secret",
    authority=BASE, disable_instance_discovery=True,
)

resources = ResourceManagementClient(
    credential, "00000000-0000-0000-0000-000000000000",
    base_url=BASE, credential_scopes=[BASE + "/.default"],
)
resources.resource_groups.create_or_update("demo", {"location": "eastus"})

blobs = BlobServiceClient(
    "https://myaccount.blob.core.azure.localhost:10443", credential=credential
)
```

Data-plane endpoints come back from the control plane (`primaryEndpoints.blob`,
`vaultUri`), so code that reads them needs no emulator-specific addresses.

### Embedded mode (Rust tests)

```rust
let emulator = azure_emulator::Emulator::new(azure_emulator::Config::default());
let service = emulator.router(); // a tower::Service; no socket, no certificate
```

Each `Emulator` is fully isolated. `core/router/tests/` shows how to drive it.

## Repository layout

```
.
├── core/
│   ├── emulator-core/   # engine (state), config, token signing, the ARM layer
│   └── router/          # the azure-emulator binary and embedded facade; tests
├── services/
│   ├── entra/           # token service
│   ├── storage/         # Microsoft.Storage + Blob data plane
│   └── keyvault/        # Microsoft.KeyVault + secrets data plane
└── tests/sdk/           # checks that run real Azure SDKs against the emulator
```

### Adding a resource provider

Resource Manager is implemented once in `core/emulator-core/src/arm.rs`:
authentication, the resource envelope, storage, listing, deletion and cascade. A
provider implements `ResourceTypeHandler` for each resource type (validation,
`properties`, actions) and registers it in `core/router/src/lib.rs`. See
`services/keyvault/src/lib.rs` for a small complete example.

A data plane is a function from a request to a response, selected by hostname in
`core/router/src/lib.rs`.

Rules carried over from the AWS emulator: never fabricate a result, never
`panic!` in a handler, and answer anything unimplemented with `501`.

## Testing

```bash
cargo test                      # in-process tests through the full router
```

To run the SDK check, start the emulator, then follow the instructions at the top
of `tests/sdk/python_smoke.py`.
