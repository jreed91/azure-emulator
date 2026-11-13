"""Drives a running emulator with the official Azure SDKs for Python.

    pip install azure-identity azure-mgmt-resource azure-mgmt-storage \
        azure-mgmt-keyvault azure-mgmt-servicebus azure-mgmt-msi \
        azure-storage-blob azure-keyvault-secrets
    REQUESTS_CA_BUNDLE=~/.azure-emulator/ca.pem python tests/sdk/python_smoke.py

The account and vault hostnames it uses must resolve to the emulator; see the
README's section on hostnames.
"""

import datetime
import os
import sys

from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.identity import ClientSecretCredential
from azure.keyvault.secrets import SecretClient
from azure.mgmt.keyvault import KeyVaultManagementClient
from azure.mgmt.msi import ManagedServiceIdentityClient
from azure.mgmt.servicebus import ServiceBusManagementClient
from azure.mgmt.resource.resources import ResourceManagementClient
from azure.mgmt.storage import StorageManagementClient
from azure.storage.blob import (
    AccountSasPermissions,
    BlobClient,
    BlobSasPermissions,
    BlobServiceClient,
    ContainerClient,
    ContainerSasPermissions,
    ResourceTypes,
    generate_account_sas,
    generate_blob_sas,
    generate_container_sas,
)

BASE = os.environ.get("AZURE_EMULATOR_URL", "https://localhost:10443")
PORT = BASE.rsplit(":", 1)[1]
SUBSCRIPTION = "00000000-0000-0000-0000-000000000000"
TENANT = "11111111-1111-1111-1111-111111111111"
GROUP, ACCOUNT, VAULT = "smoke-rg", "smokeaccount", "smoke-vault"

credential = ClientSecretCredential(
    TENANT, "smoke-app", "any-secret", authority=BASE, disable_instance_discovery=True
)
arm = {"base_url": BASE, "credential_scopes": [BASE + "/.default"]}


def step(name):
    print(f"ok  {name}")


resources = ResourceManagementClient(credential, SUBSCRIPTION, **arm)
group = resources.resource_groups.create_or_update(GROUP, {"location": "eastus", "tags": {"by": "smoke"}})
assert group.name == GROUP and group.properties.provisioning_state == "Succeeded"
assert resources.resource_groups.check_existence(GROUP)
assert [g.name for g in resources.resource_groups.list()] == [GROUP]
step("resource group create / exists / list")

storage = StorageManagementClient(credential, SUBSCRIPTION, **arm)
assert not storage.storage_accounts.check_name_availability({"name": "Bad_Name"}).name_available
account = storage.storage_accounts.begin_create(
    GROUP, ACCOUNT, {"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}}
).result()
blob_url = account.primary_endpoints.blob
assert blob_url == f"https://{ACCOUNT}.blob.core.azure.localhost:{PORT}/", blob_url
key = storage.storage_accounts.list_keys(GROUP, ACCOUNT)["keys"][0]["value"]
assert [a.name for a in storage.storage_accounts.list()] == [ACCOUNT]
step("storage account create (long-running) / list keys / list")

for label, blob_credential in [("Entra token", credential), ("shared key", key)]:
    service = BlobServiceClient(blob_url, credential=blob_credential)
    container = service.create_container(f"data-{label.split()[0].lower()}")
    try:
        service.create_container(container.container_name)
        sys.exit("duplicate container was accepted")
    except ResourceExistsError:
        pass
    blob = container.get_blob_client("folder/hello world.txt")
    blob.upload_blob(b"hello from the sdk")
    try:
        blob.upload_blob(b"again")
        sys.exit("overwrite without overwrite=True was accepted")
    except ResourceExistsError:
        pass
    blob.upload_blob(b"hello from the sdk, twice", overwrite=True, metadata={"origin": "smoke"})
    assert blob.download_blob().readall() == b"hello from the sdk, twice"
    assert blob.download_blob(offset=6, length=4).readall() == b"from"
    properties = blob.get_blob_properties()
    assert properties.size == 25 and properties.metadata == {"origin": "smoke"}
    container.upload_blob("empty", b"")
    assert container.download_blob("empty").readall() == b""
    assert [b.name for b in container.list_blobs()] == ["empty", "folder/hello world.txt"]
    walked = [b.name for b in container.walk_blobs(delimiter="/")]
    assert sorted(walked) == ["empty", "folder/"], walked
    blob.delete_blob()
    try:
        blob.get_blob_properties()
        sys.exit("deleted blob still readable")
    except ResourceNotFoundError:
        pass
    assert container.container_name in [c.name for c in service.list_containers()]
    container.delete_container()
    step(f"blob container / upload / download / range / list / delete with {label}")

assert storage.blob_services.get_service_properties(GROUP, ACCOUNT).sku.name == "Standard_LRS"
storage.blob_containers.create(GROUP, ACCOUNT, "from-arm", {"properties": {"metadata": {"made": "by-arm"}}})
arm_container = BlobServiceClient(blob_url, credential=credential).get_container_client("from-arm")
assert arm_container.get_container_properties().metadata == {"made": "by-arm"}
arm_container.upload_blob("x.txt", b"x")
BlobServiceClient(blob_url, credential=credential).create_container("from-data-plane")
assert sorted(c.name for c in storage.blob_containers.list(GROUP, ACCOUNT)) == ["from-arm", "from-data-plane"]
storage.blob_containers.delete(GROUP, ACCOUNT, "from-data-plane")
storage.queue.create(GROUP, ACCOUNT, "jobs", {})
storage.table.create(GROUP, ACCOUNT, "Customers")
share = storage.file_shares.create(GROUP, ACCOUNT, "docs", {"properties": {"shareQuota": 10}})
assert share.share_quota == 10 and share.enabled_protocols == "SMB"
scope = storage.encryption_scopes.put(GROUP, ACCOUNT, "tenant-a", {})
assert scope.state == "Enabled" and scope.source == "Microsoft.Storage"
storage.management_policies.create_or_update(
    GROUP, ACCOUNT, "default",
    {"properties": {"policy": {"rules": [{"name": "expire", "enabled": True, "type": "Lifecycle", "definition": {"actions": {"baseBlob": {"delete": {"daysAfterModificationGreaterThan": 30}}}, "filters": {"blobTypes": ["blockBlob"]}}}]}}},
)
assert storage.management_policies.get(GROUP, ACCOUNT, "default").policy.rules[0].name == "expire"
step("storage management: service properties / containers / queue / table / share / encryption scope / lifecycle policy")

# Shared access signatures the SDK signs itself, verified by the emulator.
expiry = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=1)
account_sas = generate_account_sas(
    ACCOUNT, key, ResourceTypes(service=True, container=True, object=True),
    AccountSasPermissions(read=True, write=True, delete=True, list=True, create=True), expiry,
)
sas_service = BlobServiceClient(blob_url, credential=account_sas)
sas_container = sas_service.create_container("sas")
sas_container.upload_blob("a.txt", b"signed")
assert "sas" in [c.name for c in sas_service.list_containers()]
read_only = generate_account_sas(ACCOUNT, key, ResourceTypes(object=True), AccountSasPermissions(read=True), expiry)
assert BlobClient(blob_url, "sas", "a.txt", credential=read_only).download_blob().readall() == b"signed"
blob_sas = generate_blob_sas(ACCOUNT, "sas", "a.txt", account_key=key, permission=BlobSasPermissions(read=True), expiry=expiry)
assert BlobClient(blob_url, "sas", "a.txt", credential=blob_sas).download_blob().readall() == b"signed"
container_sas = generate_container_sas(ACCOUNT, "sas", account_key=key, permission=ContainerSasPermissions(read=True, list=True), expiry=expiry)
assert [b.name for b in ContainerClient(blob_url, "sas", credential=container_sas).list_blobs()] == ["a.txt"]
refusals = [
    lambda: BlobClient(blob_url, "sas", "b.txt", credential=read_only).upload_blob(b"x"),
    lambda: BlobClient(blob_url, "sas", "other.txt", credential=blob_sas).download_blob().readall(),
    lambda: BlobClient(blob_url, "sas", "a.txt", credential=generate_blob_sas(
        ACCOUNT, "sas", "a.txt", account_key=key, permission=BlobSasPermissions(read=True),
        expiry=expiry - datetime.timedelta(hours=2))).download_blob().readall(),
    lambda: BlobClient(blob_url, "sas", "a.txt", credential=generate_blob_sas(
        ACCOUNT, "sas", "a.txt", account_key="A" * 88, permission=BlobSasPermissions(read=True), expiry=expiry)).download_blob().readall(),
]
for attempt in refusals:
    try:
        attempt()
        sys.exit("a shared access signature was accepted that should have been refused")
    except HttpResponseError as error:
        assert error.status_code == 403, error
# A token issued by Resource Manager, used by the SDK.
issued = storage.storage_accounts.list_account_sas(
    GROUP, ACCOUNT,
    {"signedServices": "b", "signedResourceTypes": "o", "signedPermission": "r", "signedExpiryTime": expiry.strftime("%Y-%m-%dT%H:%M:%SZ")},
).account_sas_token
assert BlobClient(blob_url, "sas", "a.txt", credential=issued).download_blob().readall() == b"signed"
step("shared access signatures: SDK-signed account, container and blob tokens; a token issued by Resource Manager")

try:
    BlobServiceClient(blob_url, credential="A" * 88).create_container("nope")
    sys.exit("a wrong account key was accepted")
except HttpResponseError as error:
    assert error.status_code == 403, error
step("wrong account key refused")

keyvault = KeyVaultManagementClient(credential, SUBSCRIPTION, **arm)
vault = keyvault.vaults.begin_create_or_update(
    GROUP,
    VAULT,
    {"location": "eastus", "properties": {"tenantId": TENANT, "sku": {"family": "A", "name": "standard"}, "accessPolicies": []}},
).result()
assert vault.properties.vault_uri == f"https://{VAULT}.vault.azure.localhost:{PORT}/", vault.properties.vault_uri
step("key vault create")

secrets = SecretClient(vault.properties.vault_uri, credential)
first = secrets.set_secret("api-key", "v1", content_type="text/plain")
secrets.set_secret("api-key", "v2")
assert secrets.get_secret("api-key").value == "v2"
assert secrets.get_secret("api-key", first.properties.version).value == "v1"
assert [s.name for s in secrets.list_properties_of_secrets()] == ["api-key"]
assert len(list(secrets.list_properties_of_secret_versions("api-key"))) == 2
secrets.update_secret_properties("api-key", tags={"rotated": "yes"})
assert secrets.get_secret("api-key").properties.tags == {"rotated": "yes"}
deleted = secrets.begin_delete_secret("api-key").result()
assert deleted.name == "api-key"
try:
    secrets.get_secret("api-key")
    sys.exit("deleted secret still readable")
except ResourceNotFoundError:
    pass
secrets.begin_recover_deleted_secret("api-key").result()
assert secrets.get_secret("api-key").value == "v2"
secrets.begin_delete_secret("api-key").wait()
secrets.purge_deleted_secret("api-key")
step("secret set / get / versions / update / delete / recover / purge")

keyvault.vaults.update_access_policy(
    GROUP,
    VAULT,
    "add",
    {"properties": {"accessPolicies": [{"tenantId": TENANT, "objectId": "00000000-0000-0000-0000-000000000001", "permissions": {"secrets": ["get"]}}]}},
)
assert keyvault.vaults.get(GROUP, VAULT).properties.access_policies[0].permissions.secrets == ["get"]
keyvault.secrets.create_or_update(GROUP, VAULT, "from-arm", {"properties": {"value": "set through resource manager"}})
assert secrets.get_secret("from-arm").value == "set through resource manager"
assert "from-arm" in [s.name for s in keyvault.secrets.list(GROUP, VAULT)]
keyvault.vaults.delete(GROUP, VAULT)
assert [v.name for v in keyvault.vaults.list_deleted()] == [VAULT]
assert keyvault.vaults.get_deleted(VAULT, "eastus").properties.vault_id.endswith(f"/vaults/{VAULT}")
keyvault.vaults.begin_purge_deleted(VAULT, "eastus").result()
assert not list(keyvault.vaults.list_deleted())
step("key vault access policy / management-plane secret / soft delete / purge")

servicebus = ServiceBusManagementClient(credential, SUBSCRIPTION, **arm)
BUS = "smoke-bus"
assert servicebus.namespaces.check_name_availability({"name": BUS}).name_available
bus = servicebus.namespaces.begin_create_or_update(GROUP, BUS, {"location": "eastus", "sku": {"name": "Standard"}}).result()
assert bus.provisioning_state == "Succeeded" and bus.sku.tier == "Standard", bus
assert not servicebus.namespaces.check_name_availability({"name": BUS}).name_available
keys = servicebus.namespaces.list_keys(GROUP, BUS, "RootManageSharedAccessKey")
assert keys.primary_connection_string.startswith(f"Endpoint=sb://{BUS}.servicebus.azure.localhost:{PORT}/;")
queue = servicebus.queues.create_or_update(GROUP, BUS, "jobs", {"properties": {"maxDeliveryCount": 4}})
assert queue.max_delivery_count == 4 and queue.lock_duration.total_seconds() == 60 and queue.message_count == 0
servicebus.topics.create_or_update(GROUP, BUS, "events", {})
servicebus.subscriptions.create_or_update(GROUP, BUS, "events", "audit", {})
assert servicebus.topics.get(GROUP, BUS, "events").subscription_count == 1
assert [r.name for r in servicebus.rules.list_by_subscriptions(GROUP, BUS, "events", "audit")] == ["$Default"]
rule = servicebus.rules.create_or_update(
    GROUP, BUS, "events", "audit", "errors", {"properties": {"filterType": "SqlFilter", "sqlFilter": {"sqlExpression": "level = 'error'"}}}
)
assert rule.sql_filter.compatibility_level == 20
assert sorted(q.name for q in servicebus.queues.list_by_namespace(GROUP, BUS)) == ["jobs"]
servicebus.queues.delete(GROUP, BUS, "jobs")
servicebus.namespaces.begin_delete(GROUP, BUS).result()
assert servicebus.namespaces.check_name_availability({"name": BUS}).name_available
step("service bus namespace / keys / queue / topic / subscription / rule")

primary = servicebus.namespaces.begin_create_or_update(GROUP, "smoke-primary", {"location": "eastus", "sku": {"name": "Premium"}}).result()
secondary = servicebus.namespaces.begin_create_or_update(GROUP, "smoke-secondary", {"location": "westus", "sku": {"name": "Premium"}}).result()
alias = servicebus.disaster_recovery_configs.create_or_update(
    GROUP, "smoke-primary", "smoke-alias", {"properties": {"partnerNamespace": secondary.id}}
)
assert alias.role == "Primary", alias
assert servicebus.disaster_recovery_configs.get(GROUP, "smoke-secondary", "smoke-alias").role == "Secondary"
alias_keys = servicebus.disaster_recovery_configs.list_keys(GROUP, "smoke-primary", "smoke-alias", "RootManageSharedAccessKey")
assert alias_keys.alias_primary_connection_string.startswith("Endpoint=sb://smoke-alias.servicebus.")
servicebus.disaster_recovery_configs.fail_over(GROUP, "smoke-secondary", "smoke-alias")
assert servicebus.disaster_recovery_configs.get(GROUP, "smoke-secondary", "smoke-alias").role == "PrimaryNotReplicating"
step("service bus geo-disaster-recovery alias / failover")

msi = ManagedServiceIdentityClient(credential, SUBSCRIPTION, **arm)
identity = msi.user_assigned_identities.create_or_update(GROUP, "smoke-identity", {"location": "eastus"})
assert identity.tenant_id == TENANT and len(identity.client_id) == 36
msi.federated_identity_credentials.create_or_update(
    GROUP,
    "smoke-identity",
    "github",
    {"properties": {"issuer": "https://token.actions.githubusercontent.com", "subject": "repo:o/r:ref:refs/heads/main", "audiences": ["api://AzureADTokenExchange"]}},
)
assert [c.name for c in msi.federated_identity_credentials.list(GROUP, "smoke-identity")] == ["github"]
step("user-assigned identity / federated identity credential")

resources.resource_groups.begin_delete(GROUP).result()
assert not resources.resource_groups.check_existence(GROUP)
assert storage.storage_accounts.check_name_availability({"name": ACCOUNT}).name_available
step("resource group delete (long-running) cascades")
print("all passed")
