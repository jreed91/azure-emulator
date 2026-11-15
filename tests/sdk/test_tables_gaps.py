"""Table behaviour the emulator does not match yet, asserted as Azure behaves.
Each test is a `known_gap`: it fails today and counts against the Table score."""

import datetime

import pytest
from azure.core.credentials import AzureNamedKeyCredential, AzureSasCredential
from azure.core.exceptions import HttpResponseError
from azure.data.tables import TableAccessPolicy, TableClient, TableSasPermissions, generate_table_sas


def later():
    return datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=1)


@pytest.fixture
def table(account):
    client = TableClient(account.table_url, "gaps", credential=AzureNamedKeyCredential(account.name, account.key))
    client.create_table()
    client.create_entity({"PartitionKey": "a", "RowKey": "1"})
    client.create_entity({"PartitionKey": "b", "RowKey": "1"})
    return client


@pytest.mark.known_gap("a table SAS scoped to a partition key range is verified but not narrowed to the range")
def test_sas_limited_to_a_partition_key_range(account, table):
    sas = generate_table_sas(
        AzureNamedKeyCredential(account.name, account.key),
        "gaps",
        permission=TableSasPermissions(read=True),
        expiry=later(),
        start_pk="a",
        end_pk="a",
    )
    reader = TableClient(account.table_url, "gaps", credential=AzureSasCredential(sas))
    assert reader.get_entity("a", "1")["RowKey"] == "1"
    with pytest.raises(HttpResponseError) as refused:
        reader.get_entity("b", "1")
    assert refused.value.status_code == 403


@pytest.mark.known_gap("a table SAS that names a stored access policy is refused")
def test_sas_from_a_stored_access_policy(account, table):
    table.set_table_access_policy({"readers": TableAccessPolicy(permission="r", expiry=later(), start=datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(minutes=5))})
    sas = generate_table_sas(AzureNamedKeyCredential(account.name, account.key), "gaps", policy_id="readers")
    reader = TableClient(account.table_url, "gaps", credential=AzureSasCredential(sas))
    assert reader.get_entity("a", "1")["RowKey"] == "1"
