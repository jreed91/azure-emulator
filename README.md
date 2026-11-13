# Azure Emulator

A local emulator of the Azure APIs, for building and testing without an Azure
subscription. Your SDK calls go to a program on your machine and work against
in-memory resources.

It is modelled on [aws/aws-emulator](https://github.com/aws/aws-emulator): one
binary, one port, one crate per service, all state in memory, and an honest `501`
for anything not implemented yet.

**Not every Azure API is here** (see below for what is), and what is here is
for development and tests: do not use it for production workloads, and do not
rely on it as a security boundary.

## What works today

| Area | Implemented |
|---|---|
| Entra ID (login) | OpenID Connect discovery, instance discovery, signing keys, client-credentials tokens (v1 and v2 endpoints), managed-identity (IMDS) tokens |
| Resource Manager | Bearer-token verification, `api-version` checks, subscriptions, tenants, resource groups (create, get, exists, tag, list, delete with cascade), providers, generic resource listing, cloud metadata document |
| Template deployments | ARM templates and compiled Bicep at resource-group and subscription scope: create, get, list, delete, `validate`, `whatIf`, `exportTemplate`, deployment operations, `calculateTemplateHash`. Parameters (types, allowed values, bounds, defaults, user-defined types), variables and variable loops, resource, property and output `copy` loops, conditions, child resources, `dependsOn` ordering, nested deployments (Bicep modules, inner and outer scope, other resource groups), `existing` resources, symbolic names (`languageVersion` 2.0), user-defined functions, lambdas, outputs, and complete mode. About 100 template functions plus every `list*` action, with `uniqueString` and `guid` giving the values Azure gives. See [Template deployments](#template-deployments) |
| `Microsoft.Storage` | Storage accounts with `listKeys`, `regenerateKey` and `checkNameAvailability`; blob, file, queue and table service properties; blob containers (the same store as the data plane); queues, tables and file shares (Resource Manager only); encryption scopes; lifecycle-management and inventory policies; local users; `listAccountSas` and `listServiceSas`; container legal holds, immutability policies (lock, extend, `If-Match`) and leases, enforced on the Blob data plane; file share leases; object replication policies, with blobs copied between accounts on write |
| Blob service | Containers: create, delete, properties, list. Block blobs: upload (single request), download with ranges, properties, metadata, delete, list with prefix, delimiter and paging. Authorization by Entra token, verified Shared Key, or shared access signature (account SAS and container or blob service SAS signed with an account key) |
| `Microsoft.KeyVault` | Vaults: create, get, update, delete, list, `checkNameAvailability`; soft delete with list, get, recover and purge of deleted vaults; access-policy updates; secrets through Resource Manager (the same store as the data plane); managed HSMs with soft delete, regions and name availability (no HSM data plane); keys through Resource Manager in vaults and managed HSMs (metadata only; no keys data plane yet) |
| `Microsoft.ManagedIdentity` | User-assigned identities and federated identity credentials; listing the resources that use an identity; reading any resource's system-assigned identity. Every resource's `identity` block is filled in: system-assigned identities get a principal id, and user-assigned identities are checked and given their principal and client ids |
| `Microsoft.ServiceBus` | Namespaces, queues, topics, subscriptions, rules, authorization rules with `listKeys` and `regenerateKeys`, network rule sets, geo-disaster-recovery aliases with failover and break-pairing, legacy IP filter and virtual network rules, private link resources, Standard-to-Premium migration (start and revert; completion is not modeled), `checkNameAvailability`; messaging over AMQP 1.0 (see [Service Bus messaging](#service-bus-messaging)) |
| Key Vault secrets | Set, get (latest or by version), update attributes, list, list versions, soft delete, get and list deleted, recover, purge |

Beyond these, every resource type in the Storage, Key Vault, Managed Identity and
Service Bus specifications is mounted from a generated scaffold and answers `501`
until someone implements it. `azure-emulator coverage` prints the current score:

```
Provider                           Types   Operations   Generic
Microsoft.KeyVault                 16/16        50/51         0
Microsoft.ManagedIdentity            2/2        12/14         0
Microsoft.ServiceBus               19/21        83/91         0
Microsoft.Storage                  18/32       93/153        30
Total                              55/71      238/309        30
```

Private endpoint connections and network security perimeter configurations
count as implemented: they are created only by Microsoft.Network, which the
emulator does not model, so they answer as they would in a subscription without
it (empty listings, `404` for any single one).

"Generic" counts operations on configuration-only types served from the
specification's schema alone (see *Serving a type from its schema* below); they
are not in the implemented count.

That score counts control-plane (Resource Manager) operations only; data planes
such as Blob and Key Vault secrets are not in it. Add `--operations` to list every
operation with its status.

`tests/sdk/` is a pytest suite that exercises the implemented control planes,
template deployments, Blob and secrets through the official Azure SDKs for Python, and runs in CI. No other SDK, the Azure CLI or Terraform has been
tried yet.

### Not implemented, and why

`azure-emulator coverage --operations` lists every operation still answering
`501`. Each falls into one of these groups; none is left for lack of time.

| Group | Operations | Why |
|---|---|---|
| Fixed service catalogues | `Operations_List` (all four providers), Storage `Skus_List`, `Usages_List`, `Usages_ListByLocation`, `FileServices_ListServiceUsages` / `GetServiceUsage`; Service Bus `PremiumMessagingRegions_List`, `Regions_ListBySku`, `MessagingPlan_Get` / `CreateOrUpdate` | The answer is data about Azure itself (SKUs, regions, quotas, limits, the operation catalogue) that the specification does not contain. Any answer would be invented. |
| Infrastructure the emulator does not have | Storage `Failover`, `HierarchicalNamespaceMigration`, `AbortHierarchicalNamespaceMigration`, `CustomerInitiatedMigration` / `GetCustomerInitiatedMigration`, `RestoreBlobRanges`, `FileShares_Restore`, `BlobContainers_ObjectLevelWorm`, `DeletedAccounts_List` / `Get`; Service Bus `Namespaces_Failover`, `Namespaces_Migrate`, `MigrationConfigs_CompleteMigration` | Regions to fail over between, migrations of storage layout or DNS names, blob version history, and recovery of deleted accounts and shares. Each needs a model the emulator does not have yet. |
| Other resource providers | Storage `StorageTaskAssignments_*` and their reports (Microsoft.StorageActions), the three `TestExistingConnection` / `TestProposedConnection` actions (external data sources) | They depend on services outside these four providers. |
| Not yet built | `UserAssignedIdentities_RevokeTokens`, `ContextCaches_CheckNameAvailability` | Revocation needs per-instance token revocation in the auth layer; context cache name availability needs the names' scope of uniqueness, which the specification does not state. |

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
- Service Bus messaging has no sessions (session-enabled entities refuse sends
  and receives), does not forward to topics, and answers `501` to the
  management operations it lacks (rules, sessions). Plain AMQP only (no TLS,
  no WebSockets). Tier restrictions (such as topics on a Basic namespace) are
  not enforced.
- Template deployments run to completion before the first response; there is
  no `Running` state and `cancel` always answers `409`. A `templateLink`,
  `parametersLink` or Key Vault reference in parameters answers `501`, as do
  management-group and tenant scopes and the `managementGroup()` and
  `providers()` functions. Expressions are evaluated when a resource is
  deployed, so a mistake inside a resource's `properties` fails the deployment
  instead of failing validation. `pickZones` returns no zones.
- Storage queues, tables and file shares have no data plane. Version-level
  immutability (`migrate`), and the data-plane lease operations themselves, are
  not implemented; a lease taken through Resource Manager is honoured by both.
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
`--port`, `--bind`, `--host`, `--dns-suffix`, `--amqp-port`, `--http`,
`--config`; each also has an `AZURE_EMULATOR_*` environment variable. Run with
`--help` for details. (Pick ports Node's `fetch` allows if you change them:
it refuses some, such as `10080`.)

### Docker

```bash
docker build -t azure-emulator .
docker run -p 10443:10443 -p 5672:5672 azure-emulator
```

Pushes to `main` publish `ghcr.io/<owner>/azure-emulator:edge` (and a
`sha-<commit>` tag); `v*` tags publish versioned images and release binaries.
The image listens on all interfaces and keeps its generated CA in `/data`. In
Compose, with resources created at startup from a file:

```yaml
services:
  azure:
    image: ghcr.io/<owner>/azure-emulator:edge
    environment:
      AZURE_EMULATOR_HTTP: "1"          # plain HTTP for Resource Manager
      AZURE_EMULATOR_CONFIG: /config.json
    volumes:
      - ./azure-emulator.json:/config.json:ro
    ports: ["10443:10443", "5672:5672"]
```

### Provisioning at startup

`--config <file>` (or `AZURE_EMULATOR_CONFIG`) creates resources before the
emulator starts serving, through the same Resource Manager API a client would
call, so they are validated the same way. Service Bus has a shorthand; any
other resource can be listed by id:

```json
{
  "serviceBus": [{
    "resourceGroup": "local",
    "namespace": "local-bus",
    "keys": { "RootManageSharedAccessKey": "SAS_KEY_VALUE" },
    "queues": [{ "name": "jobs", "properties": { "maxDeliveryCount": 5 } }],
    "topics": [{
      "name": "events",
      "subscriptions": [
        { "name": "all" },
        { "name": "errors", "rules": [{ "name": "errors", "properties": {
          "filterType": "SqlFilter", "sqlFilter": { "sqlExpression": "level = 'error'" } } }] }
      ]
    }]
  }],
  "resources": [{
    "id": "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/local/providers/Microsoft.Storage/storageAccounts/localfiles",
    "apiVersion": "2023-05-01",
    "body": { "location": "eastus", "kind": "StorageV2", "sku": { "name": "Standard_LRS" } }
  }]
}
```

`keys` fixes an authorization rule's keys (a string is the primary key; or
`{"primary": "...", "secondary": "..."}`), so connection strings can be written
ahead of time. A rule other than `RootManageSharedAccessKey` is created with
`Listen`, `Send` and `Manage`. Rules given for a subscription replace its
`$Default` rule. Keys can also be set at any time with `regenerateKeys` and a
`key` in the body, as on Azure.

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

### Service Bus messaging

The emulator also runs an AMQP 1.0 broker (plain AMQP, port `5672`; change it
with `--amqp-port`). The connection strings `listKeys` returns point at it and
carry `UseDevelopmentEmulator=true`, which tells the SDKs to use plain AMQP.

A connection string does not have to use the namespace's own hostname
(`<namespace>.servicebus.azure.localhost`): any name that reaches the
emulator works, such as `localhost` or a container's name. The namespace is
the host's first label when a namespace has that name, and otherwise the only
namespace there is. With keys fixed in a provisioning file, this works
without any setup call:

```
Endpoint=sb://localhost:5672/;SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=SAS_KEY_VALUE;UseDevelopmentEmulator=true
```

From Python, reading the connection string from Resource Manager:

```python
from azure.servicebus import ServiceBusClient, ServiceBusMessage

keys = servicebus_mgmt.namespaces.list_keys("rg", "orders", "RootManageSharedAccessKey")
with ServiceBusClient.from_connection_string(keys.primary_connection_string) as client:
    with client.get_queue_sender("jobs") as sender:
        sender.send_messages(ServiceBusMessage("hello"))
    with client.get_queue_receiver("jobs") as receiver:
        for message in receiver.receive_messages(max_wait_time=5):
            receiver.complete_message(message)
```

Queues, topics and subscriptions are the Resource Manager resources, read fresh
for every message, so their settings apply: lock duration, max delivery count,
time to live and dead-lettering on expiry, duplicate detection, status,
maximum message size and `forwardTo`. Supported: single and batched sends,
peek-lock (complete, abandon, defer, dead-letter, lock renewal) and
receive-and-delete, the dead-letter subqueue, scheduled messages (and
cancelling them), deferred messages by sequence number, peeking, and topic
fan-out through `$Default`, SQL filters, correlation filters and rule actions
(`SET`/`REMOVE`; a subscription gets a copy per matching rule with an action).
The SQL dialect covers comparison, `AND`/`OR`/`NOT`, arithmetic, `IS [NOT]
NULL`, `[NOT] IN`, `[NOT] LIKE` with `ESCAPE`, `EXISTS`, and `sys.`/`user.`
properties; an invalid expression is refused when the rule is created.
Shared access signatures are verified against the namespace's or entity's
authorization rule keys and their rights. Message counts show up on the queue and
subscription resources. Every abandon counts as a delivery attempt, and links
advertise the entity's maximum message size (256 KB by default), which the SDKs
size message batches by. The Python and JavaScript SDKs are both tested
against it.

The broker is built on `fe2o3-amqp`, vendored with small patches listed in
`vendor/fe2o3-amqp/PATCHED.md`.

### Template deployments

ARM templates, and Bicep compiled with `bicep build`, deploy through the usual
SDK and CLI calls:

```python
from azure.mgmt.resource.deployments import DeploymentsMgmtClient

deployments = DeploymentsMgmtClient(credential, SUBSCRIPTION, base_url=BASE, credential_scopes=[BASE + "/.default"])
result = deployments.deployments.begin_create_or_update(
    "demo", "main",
    {"properties": {"mode": "Incremental", "template": template, "parameters": {"prefix": {"value": "app"}}}},
).result()
print(result.properties.outputs)
```

The engine sends each resource through the same Resource Manager routing as a
client request. Anything the emulator implements can be deployed, and a type it
does not implement fails the deployment with that resource's own error.
`environment().suffixes` name the emulator's hostnames, so templates that build
endpoint URLs from them reach the emulator.

`core/router/tests/templates/` holds Bicep files and the templates the Bicep CLI
compiled from them; the Rust and SDK suites deploy both.

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
│   │                    #   and its template deployment engine (src/arm/deployments/)
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

### Serving a type from its schema

Many resource types are configuration objects: their behaviour is accepting
valid settings, storing them and returning them. For those, writing a handler is
unnecessary. The scaffolder emits each type's request schema into
`src/scaffold.rs`, and listing the type in `registry.register_generic` serves it
from that schema alone:

- the name is checked against the path parameter's length and pattern;
- `properties` is checked for types, closed enums, bounds, patterns and
  required members; spec defaults are filled in on creation;
- read-only members are dropped from requests, and members the spec fixes at
  creation (`x-ms-mutability`) cannot change afterwards;
- `provisioningState` is `Succeeded`, and read-only ids and creation and
  modification times are set; other read-only values are left out rather than
  invented.

Actions on these types stay `501`, and `coverage` reports them as generic, not
implemented. Opt a type in only when storing its configuration really is all the
service does with it; a type with side effects or cross-resource references needs
a handler. `services/storage/src/lib.rs` lists the types served this way.

### Checking against the specification's examples

The scaffolder also extracts every create example the specification gives into
`spec-examples.json`. `core/router/tests/all/spec_examples.rs` replays each one
for every type the emulator serves, creating parent resources from their own
examples, and checks the status and that every setting sent comes back. Examples
known to fail are listed, with the reason, in
`core/router/spec-examples-known-failures.txt`; the test fails on any other
failure and on a listed example that starts passing.

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

The SDK suite drives a real emulator process with the official Azure SDKs for
Python. Each test creates its own resource group and uniquely named resources,
so tests are independent and run in parallel; each pytest worker starts its own
emulator on a free port.

```bash
cargo build -p azure-emulator
pip install -r tests/sdk/requirements.txt
cd tests/sdk && python -m pytest -n auto
```

No hosts-file entries or certificate setup are needed: the suite resolves
`*.localhost` itself and trusts the emulator's CA. Set `AZURE_EMULATOR_BIN` to
test a binary other than `target/debug/azure-emulator`.

A smaller suite drives Service Bus with the Azure SDK for JavaScript:

```bash
cargo build -p azure-emulator
cd tests/sdk-js && npm ci && npm test
```
