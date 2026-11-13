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
| `Microsoft.Storage` | Storage accounts with `listKeys`, `regenerateKey` and `checkNameAvailability`; blob, file, queue and table service properties; blob containers (the same store as the data plane); queues, tables and file shares (Resource Manager only); encryption scopes; lifecycle-management and inventory policies; local users; `listAccountSas` and `listServiceSas` |
| Blob service | Containers: create, delete, properties, list. Block blobs: upload (single request), download with ranges, properties, metadata, delete, list with prefix, delimiter and paging. Authorization by Entra token, verified Shared Key, or shared access signature (account SAS and container or blob service SAS signed with an account key) |
| `Microsoft.KeyVault` | Vaults: create, get, update, delete, list, `checkNameAvailability`; soft delete with list, get, recover and purge of deleted vaults; access-policy updates; secrets through Resource Manager (the same store as the data plane) |
| `Microsoft.ManagedIdentity` | User-assigned identities and federated identity credentials |
| `Microsoft.ServiceBus` | Namespaces, queues, topics, subscriptions, rules, authorization rules with `listKeys` and `regenerateKeys`, network rule sets, geo-disaster-recovery aliases with failover and break-pairing, legacy IP filter and virtual network rules, private link resources, `checkNameAvailability` (control plane only: nothing serves messaging) |
| Key Vault secrets | Set, get (latest or by version), update attributes, list, list versions, soft delete, get and list deleted, recover, purge |

Beyond these, every resource type in the Storage, Key Vault, Managed Identity and
Service Bus specifications is mounted from a generated scaffold and answers `501`
until someone implements it. `azure-emulator coverage` prints the current score:

```
Provider                                  Types     Operations
Microsoft.KeyVault                         5/16          16/51
Microsoft.ManagedIdentity                   2/2          10/14
Microsoft.ServiceBus                      14/21          64/91
Microsoft.Storage                         14/32         69/153
Total                                     35/71        159/309
```

That score counts control-plane (Resource Manager) operations only; data planes
such as Blob and Key Vault secrets are not in it. Add `--operations` to list every
operation with its status.

`tests/sdk/python_smoke.py` exercises the implemented control planes, Blob and secrets through the official Azure
SDKs for Python and passes. No other SDK, the Azure CLI or Terraform has been
tried yet.

### Known gaps

- Every other Azure service and resource type.
- Role assignments and Key Vault access policies are not enforced: any valid
  token for the right audience is allowed everything.
- There is no application registry: the token service accepts any client id that
  presents a credential.
- A scaffolded type accepts exactly the `api-version`s in the specification
  repository, so a version Azure still serves but the repository has dropped is
  refused. Every accepted version gets the same behaviour.
- Operations the ARM layer cannot route yet (tenant-, location- and
  extension-scoped ones) are listed by `coverage` as `501`.
- Service Bus has no data plane: the endpoint and connection strings a namespace
  reports point at the emulator's domain, but nothing accepts AMQP there. Tier
  restrictions (such as topics on a Basic namespace) are not enforced.
- Storage queues, tables and file shares have no data plane. Container legal
  holds, immutability policies and leases through Resource Manager are stubs.
- Blob: block lists (large uploads), page and append blobs, leases, snapshots,
  copy, and conditional headers other than `If-None-Match: *`. Shared access
  signatures that name a stored access policy or use a user delegation key are
  not verified, and a token's IP and protocol restrictions are not enforced.
- Key Vault: keys, certificates, managed HSMs, private endpoints, and listing
  pages. Purge protection is stored but does not block a purge.
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
├── tools/scaffold/      # emulator-scaffold: OpenAPI specification → service scaffold
└── tests/sdk/           # checks that run real Azure SDKs against the emulator
```

Each service crate also holds two generated files: `src/scaffold.rs`, the table of
resource types, `api-version`s and operations that gets mounted, and `CONTRACT.md`,
what the specification says about each type (operations, name rules, properties).

### Adding a resource provider

Providers are scaffolded from their OpenAPI documents in
[Azure/azure-rest-api-specs](https://github.com/Azure/azure-rest-api-specs):

```bash
cargo run -p emulator-scaffold -- \
    --namespace Microsoft.ServiceBus --service servicebus \
    --spec /path/to/azure-rest-api-specs/specification/servicebus/resource-manager
```

The first run creates `services/servicebus/`, adds it to the workspace and
registers it in the router. Every run rewrites `src/scaffold.rs` and
`CONTRACT.md`; nothing hand-written is touched, so it is safe to rerun when the
specification changes. From then on every type in the provider is routable and
answers `501`.

### Implementing a resource type

Resource Manager is implemented once in `core/emulator-core/src/arm.rs`:
authentication, `api-version` checks, the resource envelope, storage, listing,
deletion and cascade. To implement a type, write a `ResourceTypeHandler` for it
(validation, `properties`, actions) and register it in the crate's `register`
before the scaffold. `CONTRACT.md` says what the type's body and name must look
like. `services/managedidentity/src/lib.rs` is a small complete example, and
`azure-emulator coverage` shows the operations move from `501` to `ok`.

An operation the generic routing cannot express (location- or tenant-scoped, or
a type whose data lives in a service's own state) is implemented by its
`operationId` with `registry.register_operation`; the path comes from the
scaffold. `services/keyvault/src/management.rs` has examples.

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
