"""Drives a running emulator with the official Azure SDKs for Python.

    pip install azure-identity azure-mgmt-resource azure-mgmt-storage \
        azure-mgmt-keyvault azure-storage-blob azure-keyvault-secrets
    REQUESTS_CA_BUNDLE=~/.azure-emulator/ca.pem python tests/sdk/python_smoke.py

The account and vault hostnames it uses must resolve to the emulator; see the
README's section on hostnames.
"""

import os
import sys

from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.identity import ClientSecretCredential
from azure.keyvault.secrets import SecretClient
from azure.mgmt.keyvault import KeyVaultManagementClient
from azure.mgmt.resource.resources import ResourceManagementClient
from azure.mgmt.storage import StorageManagementClient
from azure.storage.blob import BlobServiceClient

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

resources.resource_groups.begin_delete(GROUP).result()
assert not resources.resource_groups.check_existence(GROUP)
assert storage.storage_accounts.check_name_availability({"name": ACCOUNT}).name_available
step("resource group delete (long-running) cascades")
print("all passed")
