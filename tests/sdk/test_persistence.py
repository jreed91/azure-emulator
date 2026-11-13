"""--persist: an emulator restarted on the same state file has every
service's data back, keys and key material included."""

import hashlib
import uuid

from azure.core.credentials import AzureNamedKeyCredential
from azure.cosmos import CosmosClient
from azure.data.tables import TableClient
from azure.identity import ClientSecretCredential
from azure.keyvault.keys import KeyClient
from azure.keyvault.keys.crypto import CryptographyClient, SignatureAlgorithm
from azure.keyvault.secrets import SecretClient
from azure.mgmt.authorization import AuthorizationManagementClient
from azure.mgmt.cosmosdb import CosmosDBManagementClient
from azure.mgmt.keyvault import KeyVaultManagementClient
from azure.mgmt.resource.resources import ResourceManagementClient
from azure.mgmt.servicebus import ServiceBusManagementClient
from azure.mgmt.storage import StorageManagementClient
from azure.storage.blob import BlobServiceClient
from azure.storage.queue import QueueClient

from conftest import SUBSCRIPTION, TENANT, _free_port, run_emulator, unique

READER = "acdd72a7-3385-48ef-bd42-f606fba81ae7"


def clients(instance):
    credential = ClientSecretCredential(TENANT, "persist", "secret", authority=instance.base, disable_instance_discovery=True)
    make = lambda cls: cls(credential, SUBSCRIPTION, **instance.arm)
    return credential, make


def test_state_survives_a_restart(emulator, tmp_path):
    state = tmp_path / "state.json"
    ports = {"port": _free_port(), "amqp_port": _free_port(), "amqps_port": _free_port()}
    args = ["--persist", "--state-file", str(state)]
    group, account_name, vault_name, cosmos_name, namespace = unique("rg-"), unique("st"), unique("kv-"), unique("cosmos-"), unique("sb-")
    digest = hashlib.sha256(b"signed before the restart").digest()

    with run_emulator(emulator.home, extra_args=args, **ports) as first:
        credential, make = clients(first)
        make(ResourceManagementClient).resource_groups.create_or_update(group, {"location": "eastus", "tags": {"kept": "yes"}})
        assignment = str(uuid.uuid4())
        make(AuthorizationManagementClient).role_assignments.create(
            f"/subscriptions/{SUBSCRIPTION}/resourceGroups/{group}",
            assignment,
            {"role_definition_id": f"/subscriptions/{SUBSCRIPTION}/providers/Microsoft.Authorization/roleDefinitions/{READER}", "principal_id": "someone"},
        )

        storage = make(StorageManagementClient)
        account = storage.storage_accounts.begin_create(group, account_name, {"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}}).result()
        key = storage.storage_accounts.list_keys(group, account_name)["keys"][0]["value"]
        BlobServiceClient(account.primary_endpoints.blob, credential=key).create_container("box").upload_blob("note.txt", b"kept", metadata={"m": "1"})
        queue = QueueClient(account.primary_endpoints.queue, "jobs", credential=key)
        queue.create_queue()
        queue.send_message("pending")
        table = TableClient(account.primary_endpoints.table, "Rows", credential=AzureNamedKeyCredential(account_name, key))
        table.create_table()
        table.create_entity({"PartitionKey": "p", "RowKey": "r", "Count": 3})

        vault = make(KeyVaultManagementClient).vaults.begin_create_or_update(
            group, vault_name, {"location": "eastus", "properties": {"tenantId": TENANT, "sku": {"family": "A", "name": "standard"}, "accessPolicies": []}}
        ).result()
        SecretClient(vault.properties.vault_uri, credential).set_secret("db", "p@ss")
        signing = KeyClient(vault.properties.vault_uri, credential).create_ec_key("signer")
        signature = CryptographyClient(signing.id, credential).sign(SignatureAlgorithm.es256, digest).signature

        cosmos = make(CosmosDBManagementClient)
        cosmos.database_accounts.begin_create_or_update(
            group, cosmos_name, {"location": "eastus", "locations": [{"location_name": "eastus"}], "database_account_offer_type": "Standard"}
        ).result()
        connection = cosmos.database_accounts.list_connection_strings(group, cosmos_name).connection_strings[0].connection_string
        with CosmosClient.from_connection_string(connection, connection_verify=first.ca) as documents:
            container = documents.create_database("db").create_container("items", partition_key={"paths": ["/pk"], "kind": "Hash"})
            container.create_item({"id": "1", "pk": "a", "value": 42})

        bus = make(ServiceBusManagementClient)
        bus.namespaces.begin_create_or_update(group, namespace, {"location": "eastus"}).result()
        bus.queues.create_or_update(group, namespace, "orders", {})
        bus_key = bus.namespaces.list_keys(group, namespace, "RootManageSharedAccessKey").primary_key

    assert state.exists(), "the snapshot is written"

    with run_emulator(emulator.home, extra_args=args, **ports) as second:
        credential, make = clients(second)
        assert make(ResourceManagementClient).resource_groups.get(group).tags == {"kept": "yes"}
        names = [a.name for a in make(AuthorizationManagementClient).role_assignments.list_for_scope(f"/subscriptions/{SUBSCRIPTION}/resourceGroups/{group}")]
        assert assignment in names

        storage = make(StorageManagementClient)
        assert storage.storage_accounts.list_keys(group, account_name)["keys"][0]["value"] == key, "account keys are kept"
        account = storage.storage_accounts.get_properties(group, account_name)
        blob = BlobServiceClient(account.primary_endpoints.blob, credential=key).get_blob_client("box", "note.txt")
        assert (blob.download_blob().readall(), blob.get_blob_properties().metadata) == (b"kept", {"m": "1"})
        assert [m.content for m in QueueClient(account.primary_endpoints.queue, "jobs", credential=key).peek_messages()] == ["pending"]
        rows = TableClient(account.primary_endpoints.table, "Rows", credential=AzureNamedKeyCredential(account_name, key))
        assert rows.get_entity("p", "r")["Count"] == 3

        assert SecretClient(vault.properties.vault_uri, credential).get_secret("db").value == "p@ss"
        restored = KeyClient(vault.properties.vault_uri, credential).get_key("signer")
        assert (restored.key.x, restored.key.y) == (signing.key.x, signing.key.y), "key material is kept"
        crypto = CryptographyClient(restored.id, credential)
        assert crypto.verify(SignatureAlgorithm.es256, digest, signature).is_valid
        assert len(crypto.sign(SignatureAlgorithm.es256, digest).signature) == 64

        connection = make(CosmosDBManagementClient).database_accounts.list_connection_strings(group, cosmos_name).connection_strings[0].connection_string
        with CosmosClient.from_connection_string(connection, connection_verify=second.ca) as documents:
            assert documents.get_database_client("db").get_container_client("items").read_item("1", partition_key="a")["value"] == 42

        bus = make(ServiceBusManagementClient)
        assert bus.namespaces.list_keys(group, namespace, "RootManageSharedAccessKey").primary_key == bus_key, "SAS keys are kept"
        assert bus.queues.get(group, namespace, "orders").name == "orders"
