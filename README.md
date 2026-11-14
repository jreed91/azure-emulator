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

To see how closely each service matches Azure, check the
[fidelity scores](#fidelity-scores) published with each release on the
[releases page](https://github.com/jreed91/azure-emulator/releases). To check
whether your own project is expected to work, see
[Checking whether your project works with Azure Emulator](#checking-whether-your-project-works-with-azure-emulator).

## What works today

| Area | Implemented |
|---|---|
| Entra ID (login) | OpenID Connect discovery, instance discovery, signing keys, client-credentials tokens (v1 and v2 endpoints), managed-identity (IMDS) tokens |
| Resource Manager | Bearer-token verification, `api-version` checks, subscriptions, tenants, resource groups (create, get, exists, tag, list, delete with cascade), providers, generic resource listing (with `$filter` on type, name, location, group and tags), cloud metadata document |
| Role-based access control | All 506 built-in role definitions with Azure's ids and permissions; custom roles; role assignments at subscription, resource group and resource scope (create, get, get by id, delete, list with `atScope()` and `principalId` filters), removed with the scope they are on. Enforcement on requests inside the subscription, deployments' resources included, and on the Blob, Queue, Table, Files, Key Vault and Service Bus data planes, when started with `--enforce-rbac`; see [Role-based access control](#role-based-access-control) |
| Template deployments | ARM templates and compiled Bicep at resource-group and subscription scope: create, get, list, delete, `validate`, `whatIf`, `exportTemplate`, deployment operations, `calculateTemplateHash`. Parameters (types, allowed values, bounds, defaults, user-defined types), variables and variable loops, resource, property and output `copy` loops, conditions, child resources, `dependsOn` ordering, nested deployments (Bicep modules, inner and outer scope, other resource groups), `existing` resources, symbolic names (`languageVersion` 2.0), user-defined functions, lambdas, outputs, and complete mode. About 100 template functions plus every `list*` action, with `uniqueString` and `guid` giving the values Azure gives. See [Template deployments](#template-deployments) |
| `Microsoft.Storage` | Storage accounts with `listKeys`, `regenerateKey` and `checkNameAvailability`; blob, file, queue and table service properties; blob containers and file shares (the same stores as their data planes); encryption scopes; lifecycle-management and inventory policies; local users; `listAccountSas` and `listServiceSas`; container legal holds, immutability policies (lock, extend, `If-Match`) and leases, enforced on the Blob data plane; file share leases; object replication policies, with blobs copied between accounts on write; the data planes' service properties (logging, metrics, CORS, and blob delete retention and static website), kept per account, a set replacing only the elements it names; the Queue data plane (queues, metadata, access policies, and messages with visibility timeouts, pop receipts and time to live; Shared Key, SAS and Entra ID) on `<account>.queue.core.azure.localhost`, sharing its queues with Resource Manager; the Table data plane (tables, entities with typed properties, OData `$filter`, `$select` and `$top` queries with continuation, insert, replace, merge and upsert with `If-Match`, and entity group transactions; Shared Key, Shared Key Lite, SAS and Entra ID) on `<account>.table.core.azure.localhost`, sharing its tables with Resource Manager; the Azure Files data plane (shares with quota, metadata and stats; directories; files created at a size, written and cleared by range, downloaded with ranges, resized, with HTTP headers, metadata and range lists; names case-insensitive; Shared Key, SAS and Entra ID) on `<account>.file.core.azure.localhost`, sharing its shares with Resource Manager |
| Blob service | Containers: create, delete, properties, list. Block blobs: upload in a single request or in blocks (Put Block, Put Block List with committed, uncommitted and latest blocks, Get Block List), so the SDKs' large and parallel uploads work; download with ranges; properties, metadata, Set Blob Metadata and Set Blob Properties (content type, MD5, cache control, encoding, language, disposition); delete; list with prefix, delimiter and paging. Leases on blobs and containers (acquire, renew, change, release, break; writes to a leased blob need its id); soft delete under the account's delete retention policy (deleted blobs and their snapshots listed with `include=deleted`, restored with Undelete Blob, gone once retention ends); snapshots (create, read, list, delete with `x-ms-delete-snapshots`); Copy Blob, synchronous copy and Put Blob From URL from a blob or snapshot, from another account with a SAS or public access; `If-Match`, `If-None-Match`, `If-Modified-Since` and `If-Unmodified-Since` (304 on reads), and the `x-ms-source-` conditions on copies. Append blobs (Append Block with the max-size and append-position conditions, Seal) and page blobs (Put Page update and clear, Get Page Ranges, resize, sequence numbers and their conditions). Authorization by Entra token, verified Shared Key, or shared access signature (account SAS and container or blob service SAS signed with an account key) |
| `Microsoft.KeyVault` | Vaults: create, get, update, delete, list, `checkNameAvailability`; soft delete with list, get, recover and purge of deleted vaults; purge protection (irreversible once on), which refuses purging the vault and its deleted secrets, keys and certificates; access-policy updates; secrets through Resource Manager (the same store as the data plane); managed HSMs with soft delete, regions and name availability (no HSM data plane); keys through Resource Manager in vaults and managed HSMs, with real key material the vault's keys data plane uses |
| `Microsoft.ManagedIdentity` | User-assigned identities and federated identity credentials; listing the resources that use an identity; reading any resource's system-assigned identity. Every resource's `identity` block is filled in: system-assigned identities get a principal id, and user-assigned identities are checked and given their principal and client ids |
| `Microsoft.ServiceBus` | Namespaces, queues, topics, subscriptions, rules, authorization rules with `listKeys` and `regenerateKeys`, network rule sets, geo-disaster-recovery aliases with failover and break-pairing, legacy IP filter and virtual network rules, private link resources, Standard-to-Premium migration (start and revert; completion is not modeled), `checkNameAvailability`; messaging over AMQP 1.0 (see [Service Bus messaging](#service-bus-messaging)) |
| `Microsoft.EventHub` | Namespaces (names shared with Service Bus), event hubs, consumer groups, authorization rules with `listKeys` and `regenerateKeys`, network rule sets, `checkNameAvailability`; events over AMQP (see [Event Hubs](#event-hubs)) |
| `Microsoft.DocumentDB` | Database accounts with `listKeys`, `readonlykeys`, `listConnectionStrings` and `regenerateKey`; SQL databases and containers (the same store as the data plane, reporting dedicated throughput under `options`); the Cosmos DB for NoSQL data plane (see [Cosmos DB](#cosmos-db)) |
| Key Vault secrets | Set, get (latest or by version), update attributes, list, list versions, soft delete, get and list deleted, recover, purge |
| Key Vault keys | RSA (2048, 3072, 4096) and EC (P-256, P-384, P-521, P-256K) keys, `-HSM` types in Premium vaults: create, import (JSON Web Key), get (latest or by version), update (operations, attributes, tags), list, list versions, rotate, rotation policy (stored; rotation is not automatic), backup and restore, soft delete with get and list deleted, recover and purge. Encrypt, decrypt, wrap and unwrap with `RSA1_5`, `RSA-OAEP` and `RSA-OAEP-256`; sign and verify with `RS256`–`RS512`, `PS256`–`PS512`, `ES256`, `ES384`, `ES512` and `ES256K` over a digest; random bytes. A key's permitted operations and `enabled` are enforced, and its `nbf` and `exp`: outside them it cannot encrypt, wrap or sign, but can still decrypt, unwrap and verify |
| Key Vault certificates | Create with the `Self` issuer (signed by its own key) or the `Unknown` issuer (a CSR, completed by merging the CA's certificate); import PKCS#12 (encrypted or not) or PEM; get (latest or by version), update, list, list versions, policy, pending operation (get, cancel, delete), soft delete with get and list deleted, recover and purge, backup and restore, contacts and issuers. Each version comes with its key (RSA 2048–4096, or EC on P-256 or P-384; imported certificates may also have EC keys on P-521 or P-256K) and a secret holding the key and certificates as PKCS#12 or PEM, under the same version |

Beyond these, every resource type in the mounted providers' specifications is mounted from a generated scaffold and answers `501`
until someone implements it. `azure-emulator coverage` prints the current score:

```
Provider                           Types   Operations   Generic
Microsoft.DocumentDB                4/98       19/387         0
Microsoft.EventHub                  9/22        37/91         0
Microsoft.KeyVault                 16/16        50/51         0
Microsoft.ManagedIdentity            2/2        12/14         0
Microsoft.ServiceBus               19/21        83/91         0
Microsoft.Storage                  19/32       94/153        30
Total                             69/191      295/787        30
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

`tests/sdk/` and `tests/sdk-js/` exercise the emulator through the official
Azure SDKs for Python and JavaScript, and `tests/terraform/` applies, re-plans
and destroys a Terraform configuration with the `azurerm` provider (see
[Terraform](#terraform)), and `tests/cli/run.sh` drives every service through
the Azure CLI; all four run in CI.

### Not implemented, and why

`azure-emulator coverage --operations` lists every operation still answering
`501`. Each falls into one of these groups; none is left for lack of time.

| Group | Operations | Why |
|---|---|---|
| Fixed service catalogues | `Operations_List` (all four providers), Storage `Skus_List`, `Usages_List`, `Usages_ListByLocation`, `FileServices_ListServiceUsages` / `GetServiceUsage`; Service Bus `PremiumMessagingRegions_List`, `Regions_ListBySku`, `MessagingPlan_Get` / `CreateOrUpdate` | The answer is data about Azure itself (SKUs, regions, quotas, limits, the operation catalogue) that the specification does not contain. Any answer would be invented. |
| Infrastructure the emulator does not have | Storage `Failover`, `HierarchicalNamespaceMigration`, `AbortHierarchicalNamespaceMigration`, `CustomerInitiatedMigration`, `RestoreBlobRanges`, `FileShares_Restore`, `BlobContainers_ObjectLevelWorm`, `DeletedAccounts_List` / `Get`; Service Bus `Namespaces_Failover`, `Namespaces_Migrate`, `MigrationConfigs_CompleteMigration` | Regions to fail over between, migrations of storage layout or DNS names, blob version history, and recovery of deleted accounts and shares. Each needs a model the emulator does not have yet. |
| Other resource providers | Storage `StorageTaskAssignments_*` and their reports (Microsoft.StorageActions), the three `TestExistingConnection` / `TestProposedConnection` actions (external data sources) | They depend on services outside these four providers. |
| Not yet built | `UserAssignedIdentities_RevokeTokens`, `ContextCaches_CheckNameAvailability` | Revocation needs per-instance token revocation in the auth layer; context cache name availability needs the names' scope of uniqueness, which the specification does not state. |

### Known gaps

- Every other Azure service and resource type.
- Role assignments and Key Vault access policies are enforced only with
  `--enforce-rbac`. Cosmos DB's own data-plane role assignments
  (`sqlRoleAssignments`) are not implemented, so Cosmos accepts any valid token.
  Assignment conditions and deny assignments are not evaluated. Principal ids
  are not checked against a directory, since there is none.
- There is no application registry: the token service accepts any client id that
  presents a credential. There is no Microsoft Graph; the cloud metadata
  document names the emulator as its Graph resource, so clients that require
  one stay local.
- A scaffolded type accepts exactly the `api-version`s in the specification
  repository, so a version Azure still serves but the repository has dropped is
  refused. Every accepted version gets the same behaviour.
- Operations the ARM layer cannot route yet (tenant-, location- and
  extension-scoped ones) are listed by `coverage` as `501`.
- Service Bus throughput limits, quotas and partitioning are not modelled.
- Event Hubs has no Kafka endpoint, capture, schema registry, application
  groups or geo-replication, and does not enforce throughput units. A
  partition key maps to a partition by the emulator's own hash, so the same
  key may land on a different partition number than on Azure (it always lands
  on the same one here).
- Cosmos DB serves the NoSQL API only (no MongoDB, Cassandra, Gremlin or Table
  data planes), with one region and one partition key range per container.
  Request charges are a flat 1 RU, consistency levels are accepted but every
  read sees the latest write, and indexing policies are stored but queries
  scan. Stored procedures, triggers, user-defined functions, users,
  permissions and resource tokens, spatial functions and the all-versions
  change feed answer `501`. Entra ID tokens are accepted without checking
  data-plane role assignments.
- Template deployments run to completion before the first response; there is
  no `Running` state and `cancel` always answers `409`. A `templateLink` or
  `parametersLink` answers `501`, as do
  management-group and tenant scopes and the `managementGroup()` and
  `providers()` functions. Expressions are evaluated when a resource is
  deployed, so a mistake inside a resource's `properties` fails the deployment
  instead of failing validation. `pickZones` returns no zones.
- Azure Files: no SMB or NFS (REST only), no file copy, leases, share
  snapshots, handles or SMB permissions and attributes beyond what the REST
  API reports; files are held in memory, up to 256 MiB. Table SAS tokens scoped to key
  ranges (`spk`/`srk`/`epk`/`erk`) are verified but not narrowed, and table
  access policies are stored but not used to sign tokens. Version-level
  immutability (`migrate`) is not implemented. A container lease is the same
  lease on Resource Manager and the Blob data plane.
- Blob: versions; container soft delete; with blob soft delete on, overwritten blobs and snapshots deleted on their own (`x-ms-delete-snapshots: only` or by `snapshot=`) are not kept; page-range diffs between snapshots
  (`prevsnapshot`); page blobs are held in memory, up to 256 MiB. Copies come only
  from blobs the emulator holds (not from other URLs), always complete before
  the response, and do not check `x-ms-copy-source-authorization`. Shared access
  signatures that name a stored access policy or use a user delegation key are
  not verified, and a token's IP and protocol restrictions are not enforced.
- Key Vault: certificates from issuers other than `Self` and `Unknown` (an
  issuer can be configured but not used), certificate auto-renewal, the
  managed HSM data plane (and with it symmetric keys and AES algorithms),
  automatic rotation on a key rotation policy's schedule, key release, private
  endpoints, and listing pages. Purge protection blocks purges, but deleted
  items are not purged automatically when their retention ends.
- Some error codes and messages are best-effort and have not been compared with
  real Azure; the code marks the ones known to be unverified.
- State is kept across restarts only with `--persist`, and then without the
  messages and events in flight in Service Bus and Event Hubs (see
  [Keeping state across restarts](#keeping-state-across-restarts)).

## Fidelity scores

How closely the emulator matches Azure is its *fidelity*. Where the emulator
implements an operation, it aims for the same results as Azure for valid calls
and the same errors for invalid ones; where it does not, it answers `501`, and
anything that calls it fails straight away.

A *fidelity test* drives a workflow through an official Azure SDK and asserts
how Azure behaves: the result, the state it leaves, and for invalid calls, the
error. The fidelity tests are the Python and JavaScript SDK suites,
`tests/sdk/` and `tests/sdk-js/`; `tests/fidelity/services.json` says which
service each test file counts towards. They are written from Azure's
documentation, REST specifications and SDK behaviour, and have not all been
replayed against a live subscription, so a passing test is good evidence, not
proof, that Azure behaves the same way.

A *fidelity score* is, for each service, the number of its fidelity tests the
emulator passes out of the total, such as 40 of 50. Every release (`v*` tag)
publishes the scores at the top of its release notes, measured on the release's
commit with the tests in that commit, together with `fidelity-checks.jsonl`,
which lists every test, its service and its verdict.

Tests that pass do not regress: CI runs every one on every pull request. A
known difference from Azure is published on purpose as a failing test, marked
`@pytest.mark.known_gap("why")` in Python (or `{ todo: "known gap: why" }` in
JavaScript). It runs as a strict expected failure, so the suite stays green
while the gap is open, and fails once the emulator matches Azure, so the marker
is removed and the score goes up. Scores can still go down between releases
when new tests find differences that were already there, so compare a service's
scores together with its totals.

To reproduce the scores, or to score your own build:

```bash
cargo build -p azure-emulator
pip install -r tests/sdk/requirements.txt
(cd tests/sdk-js && npm ci)
tests/fidelity/run.sh            # writes target/fidelity/scores.md and checks.jsonl
```

The score covers what the tests exercise. `azure-emulator coverage` is a
separate measure: how many of each provider's control-plane operations are
implemented at all (see [What works today](#what-works-today)).

Every implemented control-plane operation is also exercised by at least one
Python, JavaScript or Terraform test, and CI checks this. Run with
`AZURE_EMULATOR_TRACE=<file>`, the emulator appends the operation each
Resource Manager request reached to the file. `tests/operations/check.py`
compares the suites' traces with `azure-emulator coverage --operations` and
fails on an implemented operation no test reached:

```bash
export AZURE_EMULATOR_TRACE=$PWD/target/python.trace
(cd tests/sdk && python -m pytest -n auto)
export AZURE_EMULATOR_TRACE=$PWD/target/js.trace
(cd tests/sdk-js && npm test)
python tests/operations/check.py target/python.trace target/js.trace
```

### Checking whether your project works with Azure Emulator

1. List the Azure services and operations your project calls (SDK clients and
   methods, Terraform resources, Bicep or ARM resource types).
2. Check each service's score in the release notes for the release you use,
   and look its operations up in that release's `fidelity-checks.jsonl`, or in
   the test files under `tests/sdk/` and `tests/sdk-js/`. Tests that are not
   `passed` are the known differences.
3. For control-plane operations, `azure-emulator coverage --operations` says
   whether each one is implemented or answers `501`. Read
   [Known gaps](#known-gaps) for the behaviour that is missing on purpose.
4. For anything no fidelity test covers, run your own tests against the
   emulator and against Azure, and compare the results.

If the emulator behaves differently from Azure, open an
[issue](https://github.com/jreed91/azure-emulator/issues) with the request you
sent, the response you got, and what Azure returns. Better still, add a
`known_gap` test that asserts Azure's behaviour: it becomes part of the score.

## Quick start

```bash
cargo run -p azure-emulator
```

The emulator serves HTTPS on `127.0.0.1:10443` and prints its endpoints. Options:
`--port`, `--bind`, `--host`, `--dns-suffix`, `--amqp-port`, `--http`,
`--config`, `--persist`, `--state-file`, `--enforce-rbac`, `--rbac-owner`; each also has an
`AZURE_EMULATOR_*` environment variable. Run with
`--help` for details. (Pick ports Node's `fetch` allows if you change them:
it refuses some, such as `10080`.)

### Docker

```bash
docker build -t azure-emulator .
docker run -p 10443:10443 -p 5672:5672 -p 5671:5671 azure-emulator
```

Pushes to `main` publish `ghcr.io/<owner>/azure-emulator:edge` (and a
`sha-<commit>` tag). Every commit to `main` that passes CI is also released by
[semantic-release](https://semantic-release.gitbook.io): it is tagged `v<version>`
and gets a GitHub release with binaries, fidelity scores, and the
`<version>`, `<major>.<minor>` and `latest` images. The version follows the
commit messages since the last release: a `BREAKING CHANGE:` footer or a
`type!:` subject bumps the major version, a `feat:` subject the minor version,
and any other commit the patch version (see `.releaserc.json`).
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
    ports: ["10443:10443", "5672:5672", "5671:5671"]
```

### Keeping state across restarts

By default the emulator starts empty every time. With `--persist` (or
`AZURE_EMULATOR_PERSIST=1`) it keeps its state in `state.json` in its home
directory (`AZURE_EMULATOR_HOME`, `/data` in the image), or in the file
`--state-file` (`AZURE_EMULATOR_STATE_FILE`) names:

- It loads the file at startup, before provisioning from `--config`, which
  then updates what was restored.
- It writes the file whenever the state changes (checked every two seconds)
  and again when it stops on Ctrl-C or `SIGTERM` (as `docker stop` sends).
  Each write replaces the file whole, so it is never left half-written.
- A file it cannot read (from an incompatible version, say) is moved aside to
  `state.json.unreadable`, and the emulator starts empty.

What is kept: resource groups and every resource, role assignments and custom
roles, deployments; storage account keys, blobs (with snapshots, leases and
staged blocks), queues and their messages, tables and entities; Key Vault
secrets and keys, with their key material, and deleted items; Cosmos DB
databases, containers and items; Service Bus and Event Hubs namespaces,
entities and keys. Not kept: messages and events in flight in Service Bus and
Event Hubs, which start empty again.

```yaml
services:
  azure:
    image: ghcr.io/<owner>/azure-emulator:edge
    environment:
      AZURE_EMULATOR_PERSIST: "1"
    volumes:
      - azure-data:/data
volumes:
  azure-data:
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

### Event Hubs

Event Hubs namespaces are served by the same broker as Service Bus, on the
same ports (AMQP `5672`, AMQP over TLS `5671`, WebSockets on the HTTPS port),
with the same connection strings, keys and provisioning shortcuts:

```python
from azure.eventhub import EventData, EventHubConsumerClient, EventHubProducerClient

keys = eventhub_mgmt.namespaces.list_keys("rg", "telemetry-ns", "RootManageSharedAccessKey")
with EventHubProducerClient.from_connection_string(keys.primary_connection_string, eventhub_name="devices") as producer:
    batch = producer.create_batch(partition_key="device-1")
    batch.add(EventData("reading"))
    producer.send_batch(batch)

consumer = EventHubConsumerClient.from_connection_string(
    keys.primary_connection_string, consumer_group="$Default", eventhub_name="devices")
consumer.receive(on_event=lambda partition, event: print(event.body_as_str()), starting_position="-1")
```

Each event hub's partitions are append-only logs: an event gets a sequence
number, an offset and an enqueued time, and stays until the hub's retention
passes, however many consumer groups read it. Sends go to the hub (the
partition chosen by partition key, or round robin; a batch stays together) or
to one partition. Receivers read one partition of one consumer group from any
position (earliest, latest, an offset, a sequence number or an enqueued time,
inclusive or not) and keep receiving as events arrive; a receiver with an
owner level takes the partition from receivers with a lower one. Hub and
partition properties are answered as the SDKs ask for them. Event hubs check
their partition count (fixed once created) and retention against the tier, and
a Basic namespace has only the `$Default` consumer group.

```json
{
  "eventHubs": [{
    "resourceGroup": "local",
    "namespace": "local-hubs",
    "keys": { "RootManageSharedAccessKey": "SAS_KEY_VALUE" },
    "hubs": [{ "name": "devices", "partitionCount": 4, "consumerGroups": ["analytics"] }]
  }]
}
```

### Cosmos DB

Each Cosmos DB account serves the NoSQL API on the emulator's HTTPS port, at
`https://<account>.documents.azure.localhost:10443/`. `listConnectionStrings`
returns connection strings for it, which the SDKs take as they are:

```python
from azure.cosmos import CosmosClient, PartitionKey

strings = cosmos_mgmt.database_accounts.list_connection_strings("rg", "orders-db").connection_strings
client = CosmosClient.from_connection_string(strings[0].connection_string, connection_verify=ca_path)
container = client.create_database_if_not_exists("app").create_container_if_not_exists(
    "orders", partition_key=PartitionKey(path="/tenant"))
container.upsert_item({"id": "1", "tenant": "a", "total": 10})
list(container.query_items("SELECT * FROM c WHERE c.total > 5", enable_cross_partition_query=True))
```

Supported: databases and containers (with partition keys, hierarchical keys,
unique keys and time to live); items with create, read, replace, upsert,
delete and patch (`add`, `set`, `replace`, `remove`, `incr`, `move`, with a
condition); `If-Match` and `If-None-Match`; read feeds and the incremental
change feed; transactional batches, and bulk requests whose operations each
carry their own partition key and succeed or fail independently (the SDKs'
`executeBulkOperations` and `bulk`); throughput offers; and queries.
The query language covers `SELECT` with `VALUE`, `DISTINCT` and `TOP`; `JOIN … IN`;
`WHERE` with comparison, `AND`/`OR`/`NOT`, `IN`, `BETWEEN`, `LIKE`, `??`
and `?:`; `ORDER BY`, `OFFSET … LIMIT` and `GROUP BY`; the aggregates; `EXISTS`
and scalar subqueries; parameters; and the common string, array, type-checking
and math functions. Missing properties are *undefined*, as on Cosmos DB.
Requests are signed with the account's keys (the read-only keys can only
read), or carry an Entra ID token from the emulator's token service.
Databases and containers created through the data plane show up on
Resource Manager, and the other way round.

A connection string may also point at any name that reaches the emulator
(`AccountEndpoint=https://localhost:10443/`): the account is the host's first
label when an account has that name, and otherwise the only account. With a
fixed key from a provisioning file, no setup call is needed, and the key the
Cosmos DB emulator documents works:

```json
{
  "cosmos": [{
    "resourceGroup": "local",
    "account": "local-cosmos",
    "key": "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==",
    "databases": [{ "id": "app", "containers": [{ "id": "orders", "partitionKey": "/tenant" }] }]
  }]
}
```

### Service Bus messaging

The emulator also runs an AMQP 1.0 broker (plain AMQP, port `5672`; change it
with `--amqp-port`). The connection strings `listKeys` returns point at it and
carry `UseDevelopmentEmulator=true`, which tells the SDKs to use plain AMQP.

The same broker listens over TLS on port `5671` (`--amqps-port`) with the
emulator's certificate, which is what a client uses when it connects by
namespace name, for example with an Entra ID credential from the emulator's
token service, or with a connection string without `UseDevelopmentEmulator`.
Trust the emulator's CA (`connection_verify` in Python) and resolve
`*.servicebus.azure.localhost` to the emulator:

```python
client = ServiceBusClient("orders.servicebus.azure.localhost", credential, connection_verify=ca_path)
```

AMQP over WebSockets is served on the emulator's HTTPS port at
`/$servicebus/websocket`, for clients configured with
`TransportType.AmqpOverWebsocket` and a `custom_endpoint_address` of
`https://<namespace>.servicebus.azure.localhost:10443`.

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
maximum message size and `forwardTo` (to a queue or a topic, up to four
hops). Supported: single and batched sends,
peek-lock (complete, abandon, defer, dead-letter, lock renewal) and
receive-and-delete, the dead-letter subqueue, scheduled messages (and
cancelling them), deferred messages by sequence number, and peeking.

Sessions: a session-enabled queue or subscription
takes only messages with a session id; a receiver locks a named session or the
next one with messages (waiting up to 30 seconds for one), receives only its
messages, and can renew the session lock and get or set the session state;
the lock is released when the receiver closes.

Topic fan-out works through `$Default`, SQL filters, correlation filters and rule actions
(`SET`/`REMOVE`; a subscription gets a copy per matching rule with an action).
Namespace tiers are enforced as on Azure: a Basic namespace has no topics,
sessions, duplicate detection or auto-forwarding, and only a Premium namespace
sets `maxMessageSizeInKilobytes` (1 MB to 100 MB, 1 MB by default; 256 KB
otherwise).

Rules can also be managed over AMQP (`add-rule`, `remove-rule`,
`enumerate-rules`, as the JavaScript SDK's rule manager does); they are the
same rules Resource Manager lists. Sessions can be listed
(`get-message-sessions`).
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

### Terraform

The `azurerm` provider finds the emulator through `metadata_host`, which reads
the same cloud metadata document Azure serves:

```hcl
provider "azurerm" {
  features {}
  metadata_host                   = "localhost:10443"
  subscription_id                 = "00000000-0000-0000-0000-000000000000"
  tenant_id                       = "11111111-1111-1111-1111-111111111111"
  client_id                       = "terraform"
  client_secret                   = "any-secret"
  resource_provider_registrations = "none"
}
```

Terraform is a Go program, so it needs the emulator's CA in the bundle Go
reads (`SSL_CERT_FILE`, together with the system's roots so the provider can
still be downloaded), and the hostnames of vaults and storage accounts in the
hosts file, since Go does not resolve `*.localhost` itself.
`tests/terraform/run.sh` does all of this for its configuration: a resource
group, managed identity, storage account and container, role assignment, Key
Vault with a secret and a key, Service Bus namespace and queue, and Event Hubs
namespace and hub. It applies them, checks that a second plan finds nothing to
change, and destroys them. The provider waits about two minutes after creating
a Key Vault and 20 seconds after a resource group, as it does on Azure.

### Azure CLI

Register the emulator as a cloud, turn off instance discovery (it would ask
`login.microsoftonline.com` about the emulator's authority), and sign in with
any service principal name and secret:

```bash
export REQUESTS_CA_BUNDLE=~/.azure-emulator/ca.pem   # the CA the emulator prints at startup
az cloud register -n AzureEmulator --endpoint-resource-manager https://localhost:10443/
az cloud set -n AzureEmulator
az config set core.instance_discovery=false
az login --service-principal -u my-app -p any-secret --tenant 11111111-1111-1111-1111-111111111111
az group create -n demo -l eastus
```

Data-plane commands (`az storage blob ...`, `az keyvault secret ...`) go to
per-resource hostnames under `azure.localhost`, which must resolve to
`127.0.0.1`; add them to your hosts file if your resolver does not.
`tests/cli/run.sh` is the smoke test CI runs, and lists the commands known to
work.

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
Parameters may reference Key Vault secrets (`{"reference": {"keyVault": {"id":
...}, "secretName": ..., "secretVersion": ...}}`), as parameter files and
Bicep's `getSecret()` do; as in Azure, the vault must have
`enabledForTemplateDeployment`, or the deployment is refused with
`KeyVaultParameterReferenceSecretRetrieveFailed`.
`environment().suffixes` name the emulator's hostnames, so templates that build
endpoint URLs from them reach the emulator.

`core/router/tests/templates/` holds Bicep files and the templates the Bicep CLI
compiled from them; the Rust and SDK suites deploy both.

### Role-based access control

Role assignments are always stored and served. By default they are not
enforced, so a test can sign in with any client id and do anything.
`--enforce-rbac` (or `AZURE_EMULATOR_ENFORCE_RBAC=1`) checks every request
inside the subscription against the caller's assignments, as Azure does
(tenant-level reads and provider listings are left open), answering
`403 AuthorizationFailed` with the action and scope it needed. A deployment's
resources are each checked as its caller. Someone has to be able to create
the first assignments: each `--rbac-owner <id>` (or the comma-separated
`AZURE_EMULATOR_RBAC_OWNERS`) is treated as Owner of the subscription.

Data planes are checked too, for requests signed with an Entra token. Shared
Key, shared access signatures and Service Bus SAS tokens are not subject to
role assignments, as in Azure.

| Data plane | Checked against | Refused with |
|---|---|---|
| Blob | Data actions (`.../containers/blobs/read`, `write`, `add/action`, `delete`) at the container; creating, deleting and listing containers are control-plane actions (`.../containers/write`, `delete`, `read`), as in Azure | `403 AuthorizationPermissionMismatch` |
| Queue | Data actions (`.../queues/messages/read`, `add/action` or `write`, `process/action`, `delete`) at the queue; creating, deleting and listing queues are control-plane actions (`.../queues/write`, `delete`, `read`) | `403 AuthorizationPermissionMismatch` |
| Table | Data actions (`.../tables/entities/read`, `add/action`, `update/action`, `delete`) at the table; creating, deleting and listing tables are control-plane actions | `403 AuthorizationPermissionMismatch` |
| Files | Data actions (`.../fileshares/files/read`, `write`, `delete`, ...) at the share (the SDKs send Entra requests with `x-ms-file-request-intent: backup`) | `403 AuthorizationPermissionMismatch` |
| Key Vault secrets and keys | For a vault with `enableRbacAuthorization`, data actions (`.../secrets/getSecret/action`, `setSecret`, `readMetadata`, ...; `.../keys/read`, `create/action`, `encrypt/action`, `sign/action`, `wrap/action`, ...) at the secret or key; otherwise the vault's access policies (`get`, `list`, `set`, `create`, `import`, `encrypt`, `sign`, `wrapKey`, `delete`, `recover`, `purge`, `all`, ...) | `403 Forbidden` (`ForbiddenByRbac` or `AccessDenied`) |
| Service Bus | `.../messages/send/action` and `receive/action` at the queue, topic or subscription | the link is refused for want of the `Send` or `Listen` claim |

Owner, Contributor and Reader have no data actions, so an `--rbac-owner`
cannot read blobs or secrets until it is given a data role, as in Azure.

A token's principal is its object id: the client id for the emulator's
client-credentials tokens, and for a managed identity's token, that
identity's principal id, which is what a role assignment names. The
managed-identity endpoint (`/metadata/identity/oauth2/token`) picks a
user-assigned identity by `client_id`, `object_id` or `msi_res_id`;
`ManagedIdentityCredential` reaches it with `AZURE_POD_IDENTITY_AUTHORITY_HOST`
set to the emulator's address.

```bash
azure-emulator --enforce-rbac --rbac-owner my-admin-app
```

The built-in roles come from Microsoft's published definitions;
`tools/builtin-roles/generate.py` regenerates them.

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
│   ├── keyvault/        # Microsoft.KeyVault + secrets, keys and certificates data planes
│   ├── managedidentity/ # Microsoft.ManagedIdentity
│   ├── servicebus/      # Microsoft.ServiceBus + the AMQP broker (Service Bus and Event Hubs)
│   ├── eventhub/        # Microsoft.EventHub
│   └── cosmosdb/        # Microsoft.DocumentDB + NoSQL data plane
├── tools/scaffold/      # emulator-scaffold: OpenAPI specification → service scaffold
├── vendor/fe2o3-amqp/   # the AMQP library, patched (see PATCHED.md)
├── tests/sdk/           # checks that run the Azure SDKs for Python against the emulator
├── tests/sdk-js/        # and the Azure SDKs for JavaScript
└── tests/fidelity/      # scores both suites per service (see Fidelity scores)
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

A smaller suite drives Storage (blobs, queues, tables, files), Service Bus,
Event Hubs and Cosmos DB with the Azure SDK for JavaScript:

```bash
cargo build -p azure-emulator
cd tests/sdk-js && npm ci && npm test
```

`tests/coverage.sh` runs all three (the Rust tests, and both SDK suites against
an instrumented emulator) and reports line coverage per file; `--html` also
writes a browsable report to `target/coverage/llvm-cov/html`. It builds in
`target/coverage`, apart from the normal build. It needs
[cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov) and the Python
suite's requirements (`PYTHON` picks the interpreter).
