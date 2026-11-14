"""The rest of the Storage control plane: account update and deletion, service
SAS tokens, single-resource reads and updates under an account, service
properties of the queue, table and file services, object replication
policies, the network resources Microsoft.Network would create, and the
types served from the specification's schema alone."""

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError

from conftest import unique

GENERIC_API = "2026-09-01"


def new_account(storage, group):
    name = unique("st")
    return storage.storage_accounts.begin_create(
        group, name, {"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}}
    ).result()


def test_account_update_and_delete(storage, group, account):
    updated = storage.storage_accounts.update(group, account.name, {"tags": {"env": "dev"}, "properties": {"minimumTlsVersion": "TLS1_2"}})
    assert updated.tags == {"env": "dev"} and updated.minimum_tls_version == "TLS1_2"
    assert storage.storage_accounts.get_properties(group, account.name).tags == {"env": "dev"}

    storage.storage_accounts.delete(group, account.name)
    with pytest.raises(ResourceNotFoundError):
        storage.storage_accounts.get_properties(group, account.name)
    assert storage.storage_accounts.check_name_availability({"name": account.name, "type": "Microsoft.Storage/storageAccounts"}).name_available


def test_service_sas_from_resource_manager(storage, group, account):
    from azure.storage.blob import BlobClient

    storage.blob_containers.create(group, account.name, "reports", {})
    from azure.storage.blob import BlobServiceClient

    BlobServiceClient(account.blob_url, credential=account.key).get_blob_client("reports", "q1.csv").upload_blob(b"a,b")
    token = storage.storage_accounts.list_service_sas(
        group,
        account.name,
        {
            "canonicalizedResource": f"/blob/{account.name}/reports",
            "signedResource": "c",
            "signedPermission": "r",
            "signedProtocol": "https",
            "signedExpiry": "2099-01-01T00:00:00Z",
        },
    ).service_sas_token
    blob = BlobClient(account.blob_url, "reports", "q1.csv", credential=token)
    assert blob.download_blob().readall() == b"a,b"


def test_inventory_policy_get_and_delete(storage, group, account):
    inventory = {"enabled": True, "rules": [{"name": "weekly", "enabled": True, "destination": "inventory", "definition": {"format": "Parquet", "schedule": "Weekly", "objectType": "Container", "schemaFields": ["Name"]}}]}
    storage.blob_inventory_policies.create_or_update(group, account.name, "default", {"properties": {"policy": inventory}})
    assert storage.blob_inventory_policies.get(group, account.name, "default").policy.rules[0].name == "weekly"
    storage.blob_inventory_policies.delete(group, account.name, "default")
    with pytest.raises(ResourceNotFoundError):
        storage.blob_inventory_policies.get(group, account.name, "default")


def test_encryption_scope_get(storage, group, account):
    storage.encryption_scopes.put(group, account.name, "tenant-b", {"properties": {"source": "Microsoft.Storage"}})
    got = storage.encryption_scopes.get(group, account.name, "tenant-b")
    assert (got.name, got.source, got.state) == ("tenant-b", "Microsoft.Storage", "Enabled")
    with pytest.raises(ResourceNotFoundError):
        storage.encryption_scopes.get(group, account.name, "missing")


def test_queue_and_table_get_and_update(storage, group, account):
    storage.queue.create(group, account.name, "jobs", {})
    updated = storage.queue.update(group, account.name, "jobs", {"properties": {"metadata": {"owner": "ops"}}})
    assert updated.metadata == {"owner": "ops"}
    assert storage.queue.get(group, account.name, "jobs").metadata == {"owner": "ops"}

    storage.table.create(group, account.name, "events")
    assert storage.table.get(group, account.name, "events").table_name == "events"
    storage.table.update(group, account.name, "events")
    with pytest.raises(ResourceNotFoundError):
        storage.table.get(group, account.name, "missing")


def test_queue_table_and_file_service_properties(storage, group, account):
    cors = {"corsRules": [{"allowedOrigins": ["https://app.example"], "allowedMethods": ["GET"], "allowedHeaders": ["*"], "exposedHeaders": ["*"], "maxAgeInSeconds": 60}]}

    assert [s.name for s in storage.queue_services.list(group, account.name).value] == ["default"]
    storage.queue_services.set_service_properties(group, account.name, {"properties": {"cors": cors}})
    queue = storage.queue_services.get_service_properties(group, account.name)
    assert queue.cors.cors_rules[0].allowed_origins == ["https://app.example"]

    assert [s.name for s in storage.table_services.list(group, account.name).value] == ["default"]
    table = storage.table_services.set_service_properties(group, account.name, {"properties": {"cors": cors}})
    assert table.cors.cors_rules[0].max_age_in_seconds == 60

    files = storage.file_services.set_service_properties(
        group, account.name, {"properties": {"shareDeleteRetentionPolicy": {"enabled": True, "days": 5}}}
    )
    assert (files.share_delete_retention_policy.enabled, files.share_delete_retention_policy.days) == (True, 5)
    assert storage.file_services.get_service_properties(group, account.name).share_delete_retention_policy.days == 5


def test_object_replication_policy_get_list_and_delete(storage, group):
    source, destination = new_account(storage, group), new_account(storage, group)
    for account in (source, destination):
        storage.blob_services.set_service_properties(group, account.name, {"properties": {"isVersioningEnabled": True, "changeFeed": {"enabled": True}}})
    storage.blob_containers.create(group, source.name, "outbox", {})
    storage.blob_containers.create(group, destination.name, "inbox", {})
    body = {"properties": {"sourceAccount": source.name, "destinationAccount": destination.name, "rules": [{"sourceContainer": "outbox", "destinationContainer": "inbox"}]}}
    created = storage.object_replication_policies.create_or_update(group, destination.name, "default", body)

    got = storage.object_replication_policies.get(group, destination.name, created.policy_id)
    assert (got.source_account, got.rules[0].destination_container) == (source.name, "inbox")
    assert [p.policy_id for p in storage.object_replication_policies.list(group, destination.name)] == [created.policy_id]
    storage.object_replication_policies.delete(group, destination.name, created.policy_id)
    assert list(storage.object_replication_policies.list(group, destination.name)) == []
    with pytest.raises(ResourceNotFoundError):
        storage.object_replication_policies.get(group, destination.name, created.policy_id)


def test_private_endpoint_connections_and_perimeters_are_absent(emulator, arm_request, storage, group, account):
    """Microsoft.Network creates these; without it there are none."""
    connections = storage.private_endpoint_connections
    assert list(connections.list(group, account.name)) == []
    with pytest.raises(ResourceNotFoundError):
        connections.get(group, account.name, "connection")
    with pytest.raises(HttpResponseError) as refused:
        connections.put(group, account.name, "connection", {"properties": {"privateLinkServiceConnectionState": {"status": "Approved"}}})
    assert refused.value.status_code == 404
    account_path = f"/subscriptions/{emulator.subscription}/resourceGroups/{group}/providers/Microsoft.Storage/storageAccounts/{account.name}"
    assert arm_request("DELETE", f"{account_path}/privateEndpointConnections/connection", "2025-06-01").status_code in (200, 204, 404)

    perimeters = storage.network_security_perimeter_configurations
    assert list(perimeters.list(group, account.name)) == []
    with pytest.raises(ResourceNotFoundError):
        perimeters.get(group, account.name, "association")
    reconcile = arm_request("POST", f"{account_path}/networkSecurityPerimeterConfigurations/association/reconcile", "2025-06-01", {})
    assert reconcile.status_code == 404


# Types served from the specification's schema alone: what a PUT stores, a GET
# returns and a PATCH merges into; a list holds it; a DELETE removes it.
GENERIC = [
    pytest.param(
        "advancedPlatformMetrics",
        "BlobRequestMetrics",
        {"properties": {"enabled": True, "ruleConfig": {}}},
        None,
        id="advancedPlatformMetrics",
    ),
    pytest.param(
        "blobAccessPointConfigurations",
        "archive",
        {"location": "eastus", "properties": {"description": "archive", "source": {"sourceType": "AzureBlob"}}},
        {"properties": {"description": "cold archive"}},
        id="blobAccessPointConfigurations",
    ),
    pytest.param(
        "connectors",
        "lake",
        {"location": "eastus", "properties": {"description": "lake", "dataSourceType": "Blob", "source": {"type": "AzureBlob"}}},
        {"properties": {"description": "data lake"}},
        id="connectors",
    ),
    pytest.param(
        "dataShares",
        "partners",
        {"location": "eastus", "properties": {"description": "partners", "accessPolicies": [], "assets": []}},
        {"properties": {"description": "partner share"}},
        id="dataShares",
    ),
]


@pytest.mark.parametrize("collection, name, body, patch", GENERIC)
def test_schema_served_types_under_an_account(emulator, arm_request, group, account, collection, name, body, patch):
    base = f"/subscriptions/{emulator.subscription}/resourceGroups/{group}/providers/Microsoft.Storage/storageAccounts/{account.name}/{collection}"
    created = arm_request("PUT", f"{base}/{name}", GENERIC_API, body)
    assert created.status_code in (200, 201), created.text()
    got = arm_request("GET", f"{base}/{name}", GENERIC_API)
    assert got.status_code == 200 and got.json()["name"] == name
    if patch:
        patched = arm_request("PATCH", f"{base}/{name}", GENERIC_API, patch)
        assert patched.status_code == 200, patched.text()
        assert arm_request("GET", f"{base}/{name}", GENERIC_API).json()["properties"]["description"] == patch["properties"]["description"]
    assert [r["name"] for r in arm_request("GET", base, GENERIC_API).json()["value"]] == [name]
    assert arm_request("DELETE", f"{base}/{name}", GENERIC_API).status_code in (200, 204)
    assert arm_request("GET", f"{base}/{name}", GENERIC_API).status_code == 404


def test_context_caches_and_their_containers(emulator, arm_request, group):
    caches = f"/subscriptions/{emulator.subscription}/resourceGroups/{group}/providers/Microsoft.Storage/contextCaches"
    body = {"location": "eastus", "identity": {"type": "SystemAssigned"}, "properties": {"accountKind": "Standard", "description": "prompts"}}
    name = unique("cache")
    created = arm_request("PUT", f"{caches}/{name}", GENERIC_API, body)
    assert created.status_code in (200, 201), created.text()
    assert arm_request("GET", f"{caches}/{name}", GENERIC_API).json()["properties"]["accountKind"] == "Standard"
    patched = arm_request("PATCH", f"{caches}/{name}", GENERIC_API, {"tags": {"team": "ml"}})
    assert patched.status_code == 200 and patched.json()["tags"] == {"team": "ml"}
    assert [c["name"] for c in arm_request("GET", caches, GENERIC_API).json()["value"]] == [name]
    by_subscription = arm_request("GET", f"/subscriptions/{emulator.subscription}/providers/Microsoft.Storage/contextCaches", GENERIC_API)
    assert name in [c["name"] for c in by_subscription.json()["value"]]

    containers = f"{caches}/{name}/contextCacheContainers"
    container = {"properties": {"modelName": "model-a", "provider": "OpenAI", "description": "chat"}}
    assert arm_request("PUT", f"{containers}/chat", GENERIC_API, container).status_code in (200, 201)
    assert arm_request("GET", f"{containers}/chat", GENERIC_API).json()["properties"]["modelName"] == "model-a"
    patched = arm_request("PATCH", f"{containers}/chat", GENERIC_API, {"properties": {"description": "support chat"}})
    assert patched.status_code == 200 and patched.json()["properties"]["description"] == "support chat"
    assert [c["name"] for c in arm_request("GET", containers, GENERIC_API).json()["value"]] == ["chat"]
    assert arm_request("DELETE", f"{containers}/chat", GENERIC_API).status_code in (200, 204)
    assert arm_request("GET", f"{containers}/chat", GENERIC_API).status_code == 404

    assert arm_request("DELETE", f"{caches}/{name}", GENERIC_API).status_code in (200, 204)
    assert arm_request("GET", f"{caches}/{name}", GENERIC_API).status_code == 404
