"""Table storage beyond the basics: typed filters, metadata levels, service
properties, access policies, SAS, paging and invalid entities. The raw HTTP
cases are signed with an account SAS."""

import datetime
import uuid

import pytest
import requests
from azure.core.credentials import AzureNamedKeyCredential, AzureSasCredential
from azure.core.exceptions import HttpResponseError
from azure.data.tables import (
    AccountSasPermissions,
    EdmType,
    EntityProperty,
    ResourceTypes,
    TableAccessPolicy,
    TableClient,
    TableServiceClient,
    generate_account_sas,
)


def credential(account):
    return AzureNamedKeyCredential(account.name, account.key)


def expiry():
    return datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=1)


@pytest.fixture
def table(account):
    client = TableClient(account.table_url, "typed", credential=credential(account))
    client.create_table()
    return client


ID = uuid.UUID("11111111-2222-3333-4444-555555555555")


def test_typed_filters(table):
    table.create_entity({"PartitionKey": "p", "RowKey": "a", "Id": ID, "Blob": b"\x01\x02", "Ratio": 1.5, "Count": 2, "Active": True, "Name": "O'Neil"})
    table.create_entity({"PartitionKey": "p", "RowKey": "b", "Id": uuid.uuid4(), "Blob": b"\xff", "Ratio": 3.0, "Count": 5, "Active": False, "Name": "Smith"})
    rows = lambda f, **kw: sorted(e["RowKey"] for e in table.query_entities(f, **kw))
    assert rows("Id eq @id", parameters={"id": ID}) == ["a"]
    assert rows("Id eq guid'11111111-2222-3333-4444-555555555555'") == ["a"]
    assert rows("Blob eq X'0102'") == ["a"]
    assert rows("Blob eq binary'ff'") == ["b"]
    # Int32 and Double compare by value.
    assert rows("Ratio gt 2") == ["b"]
    assert rows("Count lt 2.5") == ["a"]
    assert rows("Ratio ge 1.5 and Ratio le 1.5") == ["a"]
    # A boolean property on its own, and boolean literals.
    assert rows("Active") == ["a"]
    assert rows("not Active") == ["b"]
    assert rows("true") == ["a", "b"]
    assert rows("false") == []
    # Quotes inside a string are doubled.
    assert rows("Name eq 'O''Neil'") == ["a"]
    assert rows("Name eq @n", parameters={"n": "O'Neil"}) == ["a"]
    assert rows("RowKey ne 'a'") == ["b"]
    assert rows("Missing eq 1 or RowKey eq 'b'") == ["b"]
    with pytest.raises(HttpResponseError) as refused:
        list(table.query_entities("Count gt"))
    assert refused.value.status_code == 400
    with pytest.raises(HttpResponseError):
        list(table.query_entities("Count eq 1 )"))


def test_select_and_pages(account, table):
    for i in range(5):
        table.create_entity({"PartitionKey": "p", "RowKey": f"r{i}", "N": i})
    selected = list(table.query_entities("N ge 3", select=["N", "Missing"]))
    assert [(e["N"], e.get("Missing")) for e in selected] == [(3, None), (4, None)]
    assert [len(list(page)) for page in table.list_entities(results_per_page=2).by_page()] == [2, 2, 1]

    service = TableServiceClient(account.table_url, credential=credential(account))
    for name in ["alpha", "beta", "gamma"]:
        service.create_table(name)
    pages = [len(list(page)) for page in service.list_tables(results_per_page=2).by_page()]
    assert sum(pages) == 4 and pages[0] == 2


def test_metadata_levels(account, table):
    table.create_entity({"PartitionKey": "p", "RowKey": "r", "Big": EntityProperty(2**40, EdmType.INT64), "Name": "x"})
    sas = generate_account_sas(credential(account), ResourceTypes(object=True), AccountSasPermissions(read=True), expiry())
    url = f"{account.table_url}typed(PartitionKey='p',RowKey='r')?{sas}"
    get = lambda level: requests.get(url, headers={"Accept": f"application/json;odata={level}", "x-ms-version": "2019-02-02"})

    none = get("nometadata")
    assert none.status_code == 200, none.text
    body = none.json()
    assert not [k for k in body if k.startswith("odata.") or "@odata" in k]
    assert (body["Big"], body["Name"]) == (str(2**40), "x")

    minimal = get("minimalmetadata").json()
    assert minimal["odata.etag"] and minimal["Big@odata.type"] == "Edm.Int64"
    assert "odata.id" not in minimal

    full = get("fullmetadata").json()
    assert full["odata.type"] == f"{account.name}.typed"
    assert full["odata.id"].endswith("typed(PartitionKey='p',RowKey='r')")
    assert full["odata.editLink"] == "typed(PartitionKey='p',RowKey='r')"
    assert full["Big@odata.type"] == "Edm.Int64"


def test_invalid_entities(account, table):
    sas = generate_account_sas(credential(account), ResourceTypes(object=True), AccountSasPermissions(add=True, update=True), expiry())
    post = lambda body: requests.post(
        f"{account.table_url}typed?{sas}",
        data=body,
        headers={"Content-Type": "application/json", "Accept": "application/json;odata=nometadata", "x-ms-version": "2019-02-02"},
    )
    assert post("[1, 2]").status_code == 400
    assert post("not json").status_code == 400
    bad_type = post('{"PartitionKey": "p", "RowKey": "r", "N": "x", "N@odata.type": "Edm.Int64"}')
    assert (bad_type.status_code, bad_type.json()["odata.error"]["code"]) == (400, "InvalidInput")
    missing_key = post('{"RowKey": "r"}')
    assert missing_key.status_code == 400

    with pytest.raises(HttpResponseError) as refused:
        table.create_entity({"PartitionKey": "p", "RowKey": "wide", **{f"P{i}": i for i in range(253)}})
    assert refused.value.status_code == 400 and "TooManyProperties" in str(refused.value)
    with pytest.raises(HttpResponseError):
        table.create_entity({"PartitionKey": "p", "RowKey": "bad/key"})


def test_service_properties_and_access_policies(account, table):
    service = TableServiceClient(account.table_url, credential=credential(account))
    props = service.get_service_properties()
    assert set(props) >= {"analytics_logging", "hour_metrics", "minute_metrics", "cors"}
    service.set_service_properties(cors=[])

    policy = TableAccessPolicy(permission="r", expiry=expiry())
    table.set_table_access_policy({"readers": policy})
    assert list(table.get_table_access_policy()) == ["readers"]
    table.set_table_access_policy({})
    assert table.get_table_access_policy() == {}


def test_account_sas(account, table):
    table.create_entity({"PartitionKey": "p", "RowKey": "r"})
    read = generate_account_sas(credential(account), ResourceTypes(service=True, object=True), AccountSasPermissions(read=True, list=True), expiry())
    reader = TableServiceClient(account.table_url, credential=AzureSasCredential(read))
    assert "typed" in [t.name for t in reader.list_tables()]
    assert [e["RowKey"] for e in reader.get_table_client("typed").list_entities()] == ["r"]
    with pytest.raises(HttpResponseError) as refused:
        reader.get_table_client("typed").create_entity({"PartitionKey": "p", "RowKey": "new"})
    assert refused.value.status_code == 403
    # An account SAS for another service does not cover tables.
    blob_only = generate_account_sas(credential(account), ResourceTypes(service=True), AccountSasPermissions(list=True), expiry()).replace("ss=t", "ss=b")
    with pytest.raises(HttpResponseError):
        list(TableServiceClient(account.table_url, credential=AzureSasCredential(blob_only)).list_tables())
