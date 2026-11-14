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
    "Storage Table Data Reader": "76199698-9eea-4c19-bc75-cec21354c6b6",
    "Storage Table Data Contributor": "0a9a7e1f-b9d0-4cc4-a60d-0319b160aaa3",
    "Key Vault Secrets Officer": "b86a8fe4-44ce-4948-aee5-eccb2c155cd7",
    "Key Vault Secrets User": "4633458b-17de-408a-b874-0445c86b69e6",
    "Azure Service Bus Data Sender": "69a216fc-b8fb-44d8-bc22-1f3c2cd27a39",
    "Key Vault Crypto Officer": "14b46e9e-c2b7-41b4-b07b-48a6ebf60603",
    "Key Vault Crypto User": "12338af0-0e69-4776-bea7-57ae8d297424",
    "Key Vault Certificates Officer": "a4417e6f-fecd-4de8-b567-7b0420556985",
    "Storage Queue Data Contributor": "974c5e8b-45b9-4653-ba55-5f855dd0fb88",
    "Storage Queue Data Reader": "19e7f393-937e-4f77-808e-94535e297925",
    "Storage File Data Privileged Contributor": "69566ab7-960f-475b-8e7c-b3118f30c6bd",
    "Storage File Data Privileged Reader": "b8eda974-7b85-4f76-af95-65846b26df6d",
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


def test_table_data_roles(enforced):
    from azure.data.tables import TableServiceClient

    storage = enforced.client("azure.mgmt.storage", "StorageManagementClient")
    account = storage.storage_accounts.begin_create(
        enforced.group, unique("st"), {"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}}
    ).result()
    url = account.primary_endpoints.table
    # Creating a table is a management action: the Owner may, but has no data actions.
    admin = TableServiceClient(url, credential=enforced.admin)
    admin.create_table("orders")
    with pytest.raises(HttpResponseError):
        admin.get_table_client("orders").create_entity({"PartitionKey": "p", "RowKey": "1"})

    worker = TableServiceClient(url, credential=enforced.worker)
    with pytest.raises(HttpResponseError):
        list(worker.get_table_client("orders").list_entities())
    enforced.grant("Storage Table Data Contributor", f"{account.id}/tableServices/default/tables/orders")
    worker.get_table_client("orders").create_entity({"PartitionKey": "p", "RowKey": "1"})

    reader_app = "table-reader"
    enforced.grant("Storage Table Data Reader", account.id, principal=reader_app)
    reader = TableServiceClient(
        url, credential=ClientSecretCredential(TENANT, reader_app, "secret", authority=enforced.emulator.base, disable_instance_discovery=True)
    )
    assert [e["RowKey"] for e in reader.get_table_client("orders").list_entities()] == ["1"]
    with pytest.raises(HttpResponseError):
        reader.get_table_client("orders").create_entity({"PartitionKey": "p", "RowKey": "2"})


def test_queue_data_roles(enforced):
    from azure.storage.queue import QueueServiceClient

    storage = enforced.client("azure.mgmt.storage", "StorageManagementClient")
    account = storage.storage_accounts.begin_create(
        enforced.group, unique("st"), {"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}}
    ).result()
    url = account.primary_endpoints.queue
    # Creating a queue is a management action: the Owner may, but has no data actions.
    admin = QueueServiceClient(url, credential=enforced.admin)
    admin.create_queue("jobs")
    with pytest.raises(HttpResponseError) as refused:
        admin.get_queue_client("jobs").send_message("work")
    assert refused.value.error_code == "AuthorizationPermissionMismatch"

    worker = QueueServiceClient(url, credential=enforced.worker)
    with pytest.raises(HttpResponseError):
        worker.get_queue_client("jobs").peek_messages()
    enforced.grant("Storage Queue Data Contributor", f"{account.id}/queueServices/default/queues/jobs")
    worker.get_queue_client("jobs").send_message("work")

    reader_app = "queue-reader"
    enforced.grant("Storage Queue Data Reader", account.id, principal=reader_app)
    reader = QueueServiceClient(
        url, credential=ClientSecretCredential(TENANT, reader_app, "secret", authority=enforced.emulator.base, disable_instance_discovery=True)
    )
    assert [m.content for m in reader.get_queue_client("jobs").peek_messages()] == ["work"]
    with pytest.raises(HttpResponseError):
        reader.get_queue_client("jobs").send_message("more")


def test_file_data_roles(enforced):
    from azure.storage.fileshare import ShareServiceClient

    storage = enforced.client("azure.mgmt.storage", "StorageManagementClient")
    account = storage.storage_accounts.begin_create(
        enforced.group, unique("st"), {"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}}
    ).result()
    storage.file_shares.create(enforced.group, account.name, "docs", {})
    url = account.primary_endpoints.file
    # Entra ID on Azure Files needs the backup intent.
    worker = ShareServiceClient(url, credential=enforced.worker, token_intent="backup")
    with pytest.raises(HttpResponseError) as refused:
        worker.get_share_client("docs").get_file_client("a.txt").upload_file(b"hello")
    assert refused.value.error_code == "AuthorizationPermissionMismatch"
    enforced.grant("Storage File Data Privileged Contributor", f"{account.id}/fileServices/default/fileshares/docs")
    worker.get_share_client("docs").get_file_client("a.txt").upload_file(b"hello")

    reader_app = "file-reader"
    enforced.grant("Storage File Data Privileged Reader", account.id, principal=reader_app)
    reader = ShareServiceClient(
        url,
        credential=ClientSecretCredential(TENANT, reader_app, "secret", authority=enforced.emulator.base, disable_instance_discovery=True),
        token_intent="backup",
    )
    assert reader.get_share_client("docs").get_file_client("a.txt").download_file().readall() == b"hello"
    with pytest.raises(HttpResponseError):
        reader.get_share_client("docs").get_file_client("b.txt").upload_file(b"nope")


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


def test_key_vault_certificates_with_roles_and_policies(enforced):
    from azure.keyvault.certificates import CertificateClient, CertificatePolicy

    keyvault = enforced.client("azure.mgmt.keyvault", "KeyVaultManagementClient")
    sku = {"family": "A", "name": "standard"}
    vault = keyvault.vaults.begin_create_or_update(
        enforced.group, unique("kv-"), {"location": "eastus", "properties": {"tenantId": TENANT, "sku": sku, "enableRbacAuthorization": True}}
    ).result()
    certificates = CertificateClient(vault.properties.vault_uri, enforced.worker)
    with pytest.raises(HttpResponseError) as error:
        certificates.begin_create_certificate("tls", CertificatePolicy.get_default()).result()
    assert "ForbiddenByRbac" in str(error.value)
    enforced.grant("Key Vault Certificates Officer", vault.id)
    assert certificates.begin_create_certificate("tls", CertificatePolicy.get_default()).result().name == "tls"

    policies = keyvault.vaults.begin_create_or_update(
        enforced.group,
        unique("kv-"),
        {
            "location": "eastus",
            "properties": {
                "tenantId": TENANT,
                "sku": sku,
                "accessPolicies": [
                    {"tenantId": TENANT, "objectId": "admin", "permissions": {"certificates": ["all"]}},
                    {"tenantId": TENANT, "objectId": "worker", "permissions": {"certificates": ["get", "list"]}},
                ],
            },
        },
    ).result()
    CertificateClient(policies.properties.vault_uri, enforced.admin).begin_create_certificate("tls", CertificatePolicy.get_default()).result()
    worker = CertificateClient(policies.properties.vault_uri, enforced.worker)
    assert worker.get_certificate("tls").name == "tls"
    with pytest.raises(HttpResponseError) as error:
        worker.begin_create_certificate("mine", CertificatePolicy.get_default()).result()
    assert "does not have certificates create permission" in str(error.value)


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


def test_key_vault_keys(enforced):
    from azure.keyvault.keys import KeyClient
    from azure.keyvault.keys.crypto import CryptographyClient, EncryptionAlgorithm

    keyvault = enforced.client("azure.mgmt.keyvault", "KeyVaultManagementClient")
    vault = keyvault.vaults.begin_create_or_update(
        enforced.group,
        unique("kv-"),
        {"location": "eastus", "properties": {"tenantId": TENANT, "sku": {"family": "A", "name": "standard"}, "enableRbacAuthorization": True}},
    ).result()
    worker = KeyClient(vault.properties.vault_uri, enforced.worker)
    with pytest.raises(HttpResponseError) as error:
        worker.create_rsa_key("signing")
    assert "ForbiddenByRbac" in str(error.value)
    enforced.grant("Key Vault Crypto Officer", vault.id)
    key = worker.create_rsa_key("signing")

    # Crypto User may use a key but not create one.
    user = ClientSecretCredential(TENANT, "crypto-user", "secret", authority=enforced.emulator.base, disable_instance_discovery=True)
    enforced.grant("Key Vault Crypto User", f"{vault.id}/keys/signing", principal="crypto-user")
    crypto = CryptographyClient(key.id, user)
    sealed = crypto.encrypt(EncryptionAlgorithm.rsa_oaep_256, b"for the user")
    assert crypto.decrypt(EncryptionAlgorithm.rsa_oaep_256, sealed.ciphertext).plaintext == b"for the user"
    with pytest.raises(HttpResponseError):
        KeyClient(vault.properties.vault_uri, user).create_rsa_key("other")

    # In an access-policy vault, the policy's key permissions decide.
    policy_vault = keyvault.vaults.begin_create_or_update(
        enforced.group,
        unique("kv-"),
        {
            "location": "eastus",
            "properties": {
                "tenantId": TENANT,
                "sku": {"family": "A", "name": "standard"},
                "accessPolicies": [{"tenantId": TENANT, "objectId": "worker", "permissions": {"keys": ["create", "get"]}}],
            },
        },
    ).result()
    policy_client = KeyClient(policy_vault.properties.vault_uri, enforced.worker)
    created = policy_client.create_ec_key("ec")
    with pytest.raises(HttpResponseError) as error:
        policy_client.begin_delete_key("ec").result()
    assert "does not have keys delete permission" in str(error.value)
    assert policy_client.get_key("ec").id == created.id
