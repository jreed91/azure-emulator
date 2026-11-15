"""Key Vault behaviour the emulator does not match yet, asserted as Azure
behaves. Each test is a `known_gap`: it fails today and counts against the Key
Vault score."""

import pytest
from azure.keyvault.secrets import SecretClient


@pytest.mark.known_gap("listings are not paged: maxresults is ignored")
def test_secret_listing_pages(emulator, credential, vault):
    client = SecretClient(vault.properties.vault_uri, credential, connection_verify=emulator.ca)
    for name in ("one", "two", "three"):
        client.set_secret(name, "value")
    pages = [list(page) for page in client.list_properties_of_secrets(max_page_size=1).by_page()]
    assert [len(page) for page in pages] == [1, 1, 1]
