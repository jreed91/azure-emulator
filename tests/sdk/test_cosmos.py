"""Cosmos DB: accounts on Resource Manager, and the NoSQL data plane with azure-cosmos."""

import pytest
from azure.cosmos import CosmosClient, PartitionKey, exceptions

from conftest import unique


@pytest.fixture
def account(cosmos, group):
    name = unique("cosmos-")
    created = cosmos.database_accounts.begin_create_or_update(
        group, name, {"location": "eastus", "locations": [{"location_name": "eastus"}], "database_account_offer_type": "Standard"}
    ).result()
    return created


@pytest.fixture
def client(emulator, cosmos, group, account):
    strings = cosmos.database_accounts.list_connection_strings(group, account.name).connection_strings
    with CosmosClient.from_connection_string(strings[0].connection_string, connection_verify=emulator.ca) as client:
        yield client


def test_account_and_keys(emulator, cosmos, group, account):
    assert account.provisioning_state == "Succeeded"
    assert account.document_endpoint == f"https://{account.name}.documents.azure.localhost:{emulator.port}/"
    keys = cosmos.database_accounts.list_keys(group, account.name)
    before = keys.primary_master_key
    cosmos.database_accounts.begin_regenerate_key(group, account.name, {"keyKind": "primary"}).result()
    assert cosmos.database_accounts.list_keys(group, account.name).primary_master_key != before
    readonly = cosmos.database_accounts.list_read_only_keys(group, account.name)
    assert readonly.primary_readonly_master_key == keys.primary_readonly_master_key


def test_items(client):
    database = client.create_database_if_not_exists("app")
    container = database.create_container_if_not_exists("orders", partition_key=PartitionKey(path="/tenant"))
    container.create_item({"id": "1", "tenant": "a", "total": 10})
    with pytest.raises(exceptions.CosmosResourceExistsError):
        container.create_item({"id": "1", "tenant": "a"})
    item = container.read_item("1", partition_key="a")
    assert item["total"] == 10
    container.replace_item("1", {**item, "total": 11})
    with pytest.raises(exceptions.CosmosAccessConditionFailedError):
        container.replace_item("1", {**item, "total": 12}, etag=item["_etag"], match_condition=__import__("azure.core", fromlist=["MatchConditions"]).MatchConditions.IfNotModified)
    container.upsert_item({"id": "2", "tenant": "a", "total": 30})
    patched = container.patch_item("1", partition_key="a", patch_operations=[{"op": "incr", "path": "/total", "value": 4}])
    assert patched["total"] == 15
    container.delete_item("2", partition_key="a")
    with pytest.raises(exceptions.CosmosResourceNotFoundError):
        container.read_item("2", partition_key="a")
    assert [i["id"] for i in container.read_all_items()] == ["1"]


def test_queries(client):
    container = client.create_database_if_not_exists("app").create_container_if_not_exists("people", partition_key=PartitionKey(path="/city"))
    for id_, city, age in [("ann", "Oslo", 31), ("bob", "Rome", 25), ("cid", "Oslo", 40)]:
        container.create_item({"id": id_, "city": city, "age": age, "tags": ["x"] if age > 30 else []})
    oslo = container.query_items("SELECT * FROM c WHERE c.age > @min", parameters=[{"name": "@min", "value": 30}], partition_key="Oslo")
    assert sorted(i["id"] for i in oslo) == ["ann", "cid"]
    ordered = container.query_items("SELECT VALUE c.id FROM c ORDER BY c.age DESC", enable_cross_partition_query=True)
    assert list(ordered) == ["cid", "ann", "bob"]
    assert list(container.query_items("SELECT VALUE COUNT(1) FROM c", enable_cross_partition_query=True)) == [3]
    grouped = container.query_items("SELECT c.city, COUNT(1) AS n FROM c GROUP BY c.city", enable_cross_partition_query=True)
    assert sorted((g["city"], g["n"]) for g in grouped) == [("Oslo", 2), ("Rome", 1)]
    joined = container.query_items("SELECT c.id, t FROM c JOIN t IN c.tags", enable_cross_partition_query=True)
    assert sorted(r["id"] for r in joined) == ["ann", "cid"]
    with pytest.raises(exceptions.CosmosHttpResponseError):
        list(container.query_items("SELECT * FROM c WHERE", enable_cross_partition_query=True))


def test_containers_on_resource_manager_and_the_data_plane(cosmos, group, account, client):
    cosmos.sql_resources.begin_create_update_sql_database(group, account.name, "managed", {"properties": {"resource": {"id": "managed"}}}).result()
    cosmos.sql_resources.begin_create_update_sql_container(
        group, account.name, "managed", "events", {"properties": {"resource": {"id": "events", "partitionKey": {"paths": ["/kind"], "kind": "Hash"}}}}
    ).result()
    container = client.get_database_client("managed").get_container_client("events")
    container.create_item({"id": "e1", "kind": "click"})
    assert container.read()["partitionKey"]["paths"] == ["/kind"]

    client.create_database("datadb")
    assert "datadb" in [d.name for d in cosmos.sql_resources.list_sql_databases(group, account.name)]
    cosmos.sql_resources.begin_delete_sql_database(group, account.name, "datadb").result()
    with pytest.raises(exceptions.CosmosResourceNotFoundError):
        client.get_database_client("datadb").read()


def test_wrong_key_is_refused(emulator, cosmos, group, account):
    endpoint = account.document_endpoint
    with pytest.raises(exceptions.CosmosHttpResponseError) as refused:
        with CosmosClient(endpoint, credential="A" * 86 + "==", connection_verify=emulator.ca) as forged:
            list(forged.list_databases())
    assert refused.value.status_code == 401
