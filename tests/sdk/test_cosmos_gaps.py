"""Cosmos DB behaviour the emulator does not match yet, asserted as Azure
behaves. Each test is a `known_gap`: it fails today and counts against the
Cosmos DB score."""

import pytest
from azure.cosmos import CosmosClient, PartitionKey, exceptions

from test_cosmos import account, client  # noqa: F401  (fixtures)


@pytest.mark.known_gap("stored procedures answer 501")
def test_stored_procedure(client):  # noqa: F811
    container = client.create_database_if_not_exists("app").create_container_if_not_exists("procs", partition_key=PartitionKey(path="/pk"))
    container.scripts.create_stored_procedure({"id": "hello", "body": "function () { getContext().getResponse().setBody('hi'); }"})
    assert container.scripts.execute_stored_procedure("hello", partition_key="a") == "hi"


@pytest.mark.known_gap("an Entra ID token is accepted without a data-plane role assignment")
def test_entra_token_needs_a_data_role(emulator, credential, account):  # noqa: F811
    entra = CosmosClient(account.document_endpoint, credential, connection_verify=emulator.ca)
    with pytest.raises(exceptions.CosmosHttpResponseError) as refused:
        list(entra.list_databases())
    assert refused.value.status_code == 403
