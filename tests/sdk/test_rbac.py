"""Role assignments enforced on the data planes. These tests run their own
emulator with --enforce-rbac, where the app `admin` is subscription Owner and
the app `worker` starts with no roles."""

import uuid

import pytest
from azure.core.exceptions import HttpResponseError
from azure.identity import ClientSecretCredential

from conftest import SUBSCRIPTION, TENANT, run_emulator, unique

ROLES = {
    "Storage Blob Data Contributor": "ba92f5b4-2d11-453d-a403-e96b0029c9fe",
    "Key Vault Secrets Officer": "b86a8fe4-44ce-4948-aee5-eccb2c155cd7",
    "Key Vault Secrets User": "4633458b-17de-408a-b874-0445c86b69e6",
    "Azure Service Bus Data Sender": "69a216fc-b8fb-44d8-bc22-1f3c2cd27a39",
}


class Enforced:
    def __init__(self, emulator):
        self.emulator = emulator
        credential = lambda app: ClientSecretCredential(TENANT, app, "secret", authority=emulator.base, disable_instance_discovery=True)
        self.admin = credential("admin")
        self.worker = credential("worker")
        self.group = unique("rg-")
        self.resources.resource_groups.create_or_update(self.group, {"location": "eastus"})

    def client(self, module, name, credential=None):
        cls = getattr(__import__(module, fromlist=[name]), name)
        return cls(credential or self.admin, SUBSCRIPTION, **self.emulator.arm)

    @property
    def resources(self):
        return self.client("azure.mgmt.resource.resources", "ResourceManagementClient")

    def grant(self, role, scope, principal="worker"):
        authorization = self.client("azure.mgmt.authorization", "AuthorizationManagementClient")
        authorization.role_assignments.create(
            scope,
            str(uuid.uuid4()),
            {
                "role_definition_id": f"/subscriptions/{SUBSCRIPTION}/providers/Microsoft.Authorization/roleDefinitions/{ROLES[role]}",
                "principal_id": principal,
            },
        )


@pytest.fixture(scope="module")
def enforced(emulator):
    with run_emulator(emulator.home, extra_args=["--enforce-rbac", "--rbac-owner", "admin"]) as instance:
        yield Enforced(instance)


def test_management_needs_a_role(enforced):
    from azure.mgmt.resource.resources import ResourceManagementClient

    worker = ResourceManagementClient(enforced.worker, SUBSCRIPTION, **enforced.emulator.arm)
    with pytest.raises(HttpResponseError) as error:
        worker.resource_groups.get(enforced.group)
    assert error.value.error.code == "AuthorizationFailed"


def test_blob_data_roles(enforced):
    from azure.storage.blob import BlobServiceClient

    storage = enforced.client("azure.mgmt.storage", "StorageManagementClient")
    name = unique("st")
    account = storage.storage_accounts.begin_create(
        enforced.group, name, {"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}}
    ).result()
    url = account.primary_endpoints.blob
    # Creating containers is a management action: the Owner may.
    admin = BlobServiceClient(url, credential=enforced.admin)
    admin.create_container("mine")
    admin.create_container("theirs")
    # Owner has no data actions, so it cannot write blobs, as in Azure.
    with pytest.raises(HttpResponseError) as error:
        admin.get_blob_client("mine", "a.txt").upload_blob(b"x")
    assert error.value.error_code == "AuthorizationPermissionMismatch"

    worker = BlobServiceClient(url, credential=enforced.worker)
    with pytest.raises(HttpResponseError):
        worker.get_blob_client("mine", "a.txt").upload_blob(b"x")
    enforced.grant("Storage Blob Data Contributor", f"{account.id}/blobServices/default/containers/mine")
    worker.get_blob_client("mine", "a.txt").upload_blob(b"hello")
    assert worker.get_blob_client("mine", "a.txt").download_blob().readall() == b"hello"
    assert [b.name for b in worker.get_container_client("mine").list_blobs()] == ["a.txt"]
    # Only in the container the role is on.
    with pytest.raises(HttpResponseError):
        worker.get_blob_client("theirs", "a.txt").upload_blob(b"x")


def test_key_vault_with_role_based_access(enforced):
    from azure.keyvault.secrets import SecretClient

    keyvault = enforced.client("azure.mgmt.keyvault", "KeyVaultManagementClient")
    vault = keyvault.vaults.begin_create_or_update(
        enforced.group,
        unique("kv-"),
        {"location": "eastus", "properties": {"tenantId": TENANT, "sku": {"family": "A", "name": "standard"}, "enableRbacAuthorization": True}},
    ).result()
    secrets = SecretClient(vault.properties.vault_uri, enforced.worker)
    with pytest.raises(HttpResponseError) as error:
        secrets.set_secret("db", "p@ss")
    assert "ForbiddenByRbac" in str(error.value)

    enforced.grant("Key Vault Secrets Officer", vault.id)
    secrets.set_secret("db", "p@ss")

    # Secrets User may read a value but not write one.
    reader_app = "reader"
    enforced.grant("Key Vault Secrets User", vault.id, principal=reader_app)
    reader = SecretClient(
        vault.properties.vault_uri,
        ClientSecretCredential(TENANT, reader_app, "secret", authority=enforced.emulator.base, disable_instance_discovery=True),
    )
    assert reader.get_secret("db").value == "p@ss"
    with pytest.raises(HttpResponseError):
        reader.set_secret("db", "changed")


def test_key_vault_with_access_policies(enforced):
    from azure.keyvault.secrets import SecretClient

    keyvault = enforced.client("azure.mgmt.keyvault", "KeyVaultManagementClient")
    vault = keyvault.vaults.begin_create_or_update(
        enforced.group,
        unique("kv-"),
        {
            "location": "eastus",
            "properties": {
                "tenantId": TENANT,
                "sku": {"family": "A", "name": "standard"},
                "accessPolicies": [
                    {"tenantId": TENANT, "objectId": "admin", "permissions": {"secrets": ["all"]}},
                    {"tenantId": TENANT, "objectId": "worker", "permissions": {"secrets": ["get"]}},
                ],
            },
        },
    ).result()
    SecretClient(vault.properties.vault_uri, enforced.admin).set_secret("token", "t0k3n")
    worker = SecretClient(vault.properties.vault_uri, enforced.worker)
    assert worker.get_secret("token").value == "t0k3n"
    with pytest.raises(HttpResponseError) as error:
        worker.set_secret("token", "mine")
    assert "does not have secrets set permission" in str(error.value)


def test_service_bus_data_roles(enforced):
    from azure.servicebus import ServiceBusClient, ServiceBusMessage, TransportType
    from azure.servicebus.exceptions import ServiceBusError

    servicebus = enforced.client("azure.mgmt.servicebus", "ServiceBusManagementClient")
    namespace = servicebus.namespaces.begin_create_or_update(enforced.group, unique("sb-"), {"location": "eastus"}).result()
    queue = servicebus.queues.create_or_update(enforced.group, namespace.name, "jobs", {})
    host = f"{namespace.name}.servicebus.azure.localhost"

    def client():
        return ServiceBusClient(
            host,
            enforced.worker,
            transport_type=TransportType.AmqpOverWebsocket,
            custom_endpoint_address=f"https://{host}:{enforced.emulator.port}",
            connection_verify=enforced.emulator.ca,
            retry_total=0,
        )

    with client() as c, pytest.raises(ServiceBusError):
        with c.get_queue_sender("jobs") as sender:
            sender.send_messages(ServiceBusMessage("refused"))

    enforced.grant("Azure Service Bus Data Sender", queue.id)
    with client() as c:
        with c.get_queue_sender("jobs") as sender:
            sender.send_messages(ServiceBusMessage("allowed"))
    # Sender cannot receive.
    with client() as c, pytest.raises(ServiceBusError):
        with c.get_queue_receiver("jobs", max_wait_time=1) as receiver:
            receiver.receive_messages(max_message_count=1, max_wait_time=1)
