"""Table storage: the data plane, with Shared Key, Entra ID and SAS."""

import datetime
import uuid

import pytest
from azure.core.credentials import AzureNamedKeyCredential, AzureSasCredential
from azure.core.exceptions import ClientAuthenticationError, HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.data.tables import (
    EdmType,
    EntityProperty,
    TableClient,
    TableSasPermissions,
    TableServiceClient,
    TableTransactionError,
    UpdateMode,
    generate_table_sas,
)
from azure.core import MatchConditions


def key_credential(account):
    return AzureNamedKeyCredential(account.name, account.key)


@pytest.mark.live
@pytest.mark.parametrize("auth", ["shared-key", "entra"])
def test_tables(credential, account, auth):
    service = TableServiceClient(account.table_url, credential=key_credential(account) if auth == "shared-key" else credential)
    service.create_table("People")
    with pytest.raises(ResourceExistsError):
        service.create_table("people")
    service.create_table_if_not_exists("Orders")
    assert sorted(t.name for t in service.list_tables()) == ["Orders", "People"]
    assert [t.name for t in service.query_tables("TableName eq 'Orders'")] == ["Orders"]
    service.delete_table("Orders")
    assert [t.name for t in service.list_tables()] == ["People"]
    with pytest.raises(ResourceNotFoundError):
        service.get_table_client("Orders").create_entity({"PartitionKey": "a", "RowKey": "b"})


def test_entities(account):
    table = TableClient(account.table_url, "inventory", credential=key_credential(account))
    table.create_table()
    when = datetime.datetime(2024, 5, 1, 12, 30, tzinfo=datetime.timezone.utc)
    ident = uuid.uuid4()
    created = table.create_entity({
        "PartitionKey": "tools", "RowKey": "hammer", "Count": 12, "Price": 9.5, "Big": EntityProperty(2**40, EdmType.INT64),
        "Stocked": when, "Id": ident, "Blob": b"\x00\x01", "Active": True, "Name": "Claw hammer",
    })
    assert created["etag"].startswith('W/"datetime')
    with pytest.raises(ResourceExistsError):
        table.create_entity({"PartitionKey": "tools", "RowKey": "hammer"})

    got = table.get_entity("tools", "hammer")
    assert (got["Count"], got["Price"], got["Big"].value, got["Stocked"], got["Id"], got["Blob"], got["Active"]) == (12, 9.5, 2**40, when, ident, b"\x00\x01", True)
    assert got.metadata["etag"] == created["etag"]

    for row, count in [("saw", 3), ("drill", 40), ("level", 7)]:
        table.create_entity({"PartitionKey": "tools", "RowKey": row, "Count": count, "Active": count > 5})
    table.create_entity({"PartitionKey": "paint", "RowKey": "red", "Count": 1})

    rows = lambda f, **kw: sorted(e["RowKey"] for e in table.query_entities(f, **kw))
    assert rows("PartitionKey eq 'tools' and Count gt 5") == ["drill", "hammer", "level"]
    assert rows("Count le @n or RowKey eq 'red'", parameters={"n": 3}) == ["red", "saw"]
    assert rows("Stocked ge @t", parameters={"t": when - datetime.timedelta(days=1)}) == ["hammer"]
    assert rows("Big gt 1000000L") == ["hammer"]
    assert rows("Active eq true and not (RowKey eq 'drill')") == ["hammer", "level"]
    assert [sorted(e.keys()) for e in table.query_entities("RowKey eq 'saw'", select=["Count"])] == [["Count"]]

    pages = table.list_entities(results_per_page=2).by_page()
    assert [len(list(page)) for page in pages] == [2, 2, 1]

    table.update_entity({"PartitionKey": "tools", "RowKey": "saw", "Sharp": True}, mode=UpdateMode.MERGE)
    assert (table.get_entity("tools", "saw")["Count"], table.get_entity("tools", "saw")["Sharp"]) == (3, True)
    table.update_entity({"PartitionKey": "tools", "RowKey": "saw", "Sharp": False}, mode=UpdateMode.REPLACE)
    assert "Count" not in table.get_entity("tools", "saw")

    current = table.get_entity("tools", "level")
    table.update_entity({"PartitionKey": "tools", "RowKey": "level", "Count": 8}, etag=current.metadata["etag"], match_condition=MatchConditions.IfNotModified)
    with pytest.raises(HttpResponseError) as stale:
        table.update_entity({"PartitionKey": "tools", "RowKey": "level", "Count": 9}, etag=current.metadata["etag"], match_condition=MatchConditions.IfNotModified)
    assert stale.value.status_code == 412
    with pytest.raises(ResourceNotFoundError):
        table.update_entity({"PartitionKey": "tools", "RowKey": "missing", "Count": 1})
    table.upsert_entity({"PartitionKey": "tools", "RowKey": "missing", "Count": 1})
    table.delete_entity("tools", "missing")
    table.delete_entity("tools", "missing")  # deleting what is gone is not an error for the SDK
    with pytest.raises(ResourceNotFoundError):
        table.get_entity("tools", "missing")


def test_transactions(account):
    table = TableClient(account.table_url, "ledger", credential=key_credential(account))
    table.create_table()
    table.create_entity({"PartitionKey": "acct", "RowKey": "2", "Amount": 5})
    results = table.submit_transaction([
        ("create", {"PartitionKey": "acct", "RowKey": "1", "Amount": 10}),
        ("upsert", {"PartitionKey": "acct", "RowKey": "3", "Amount": 1}),
        ("update", {"PartitionKey": "acct", "RowKey": "2", "Amount": 6}, {"mode": "merge"}),
    ])
    assert len(results) == 3 and all("etag" in r for r in results)
    assert sorted((e["RowKey"], e["Amount"]) for e in table.list_entities()) == [("1", 10), ("2", 6), ("3", 1)]

    with pytest.raises(TableTransactionError):
        table.submit_transaction([
            ("delete", {"PartitionKey": "acct", "RowKey": "1"}),
            ("create", {"PartitionKey": "acct", "RowKey": "2", "Amount": 0}),
        ])
    assert len(list(table.list_entities())) == 3, "a failed transaction changes nothing"


def test_sas_and_resource_manager(storage, group, account):
    TableClient(account.table_url, "events", credential=key_credential(account)).create_table()
    expiry = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=1)
    read_only = generate_table_sas(key_credential(account), "events", permission=TableSasPermissions(read=True), expiry=expiry)
    reader = TableClient(account.table_url, "events", credential=AzureSasCredential(read_only))
    assert list(reader.list_entities()) == []
    with pytest.raises(HttpResponseError):
        reader.create_entity({"PartitionKey": "a", "RowKey": "b"})

    assert [t.name for t in storage.table.list(group, account.name)] == ["events"]
    storage.table.create(group, account.name, "audit")
    service = TableServiceClient(account.table_url, credential=key_credential(account))
    assert sorted(t.name for t in service.list_tables()) == ["audit", "events"]
    storage.table.delete(group, account.name, "audit")
    assert [t.name for t in service.list_tables()] == ["events"]

    forged = TableServiceClient(account.table_url, credential=AzureNamedKeyCredential(account.name, "A" * 86 + "=="))
    with pytest.raises(ClientAuthenticationError):
        forged.create_table("nope")
