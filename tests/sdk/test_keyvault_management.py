"""Key Vault on Resource Manager: listing and updating vaults, access policy
operations, name availability, validation, and recovering a soft-deleted
vault with its contents."""

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.keyvault.secrets import SecretClient

from conftest import TENANT, unique

SKU = {"family": "A", "name": "standard"}


def create(keyvault, group, name, **properties):
    body = {"location": "eastus", "properties": {"tenantId": TENANT, "sku": SKU, "accessPolicies": [], **properties}}
    return keyvault.vaults.begin_create_or_update(group, name, body).result()


def test_list_and_update(keyvault, group):
    first, second = unique("kv-"), unique("kv-")
    create(keyvault, group, first)
    create(keyvault, group, second)
    assert sorted(v.name for v in keyvault.vaults.list_by_resource_group(group)) == sorted([first, second])
    assert {first, second} <= {v.name for v in keyvault.vaults.list_by_subscription()}
    updated = keyvault.vaults.update(group, first, {"tags": {"env": "test"}, "properties": {"enableRbacAuthorization": True}})
    assert (updated.tags, updated.properties.enable_rbac_authorization) == ({"env": "test"}, True)
    assert keyvault.vaults.get(group, first).properties.enable_rbac_authorization is True


def test_access_policy_operations(emulator, keyvault, group):
    name = unique("kv-")
    create(keyvault, group, name)
    policy = lambda oid, secrets: {"tenantId": TENANT, "objectId": oid, "permissions": {"secrets": secrets}}
    policies = lambda: sorted((p.object_id, tuple(p.permissions.secrets or [])) for p in keyvault.vaults.get(group, name).properties.access_policies)
    keyvault.vaults.update_access_policy(group, name, "add", {"properties": {"accessPolicies": [policy("a", ["get"]), policy("b", ["list"])]}})
    assert policies() == [("a", ("get",)), ("b", ("list",))]
    # Adding to an existing principal merges permissions.
    keyvault.vaults.update_access_policy(group, name, "add", {"properties": {"accessPolicies": [policy("a", ["set"])]}})
    assert dict(policies())["a"] == ("get", "set")
    keyvault.vaults.update_access_policy(group, name, "replace", {"properties": {"accessPolicies": [policy("a", ["delete"])]}})
    assert dict(policies())["a"] == ("delete",)
    keyvault.vaults.update_access_policy(group, name, "remove", {"properties": {"accessPolicies": [policy("b", ["list"])]}})
    assert [oid for oid, _ in policies()] == ["a"]


def test_name_availability_and_validation(keyvault, group):
    name = unique("kv-")
    assert keyvault.vaults.check_name_availability({"name": name, "type": "Microsoft.KeyVault/vaults"}).name_available
    create(keyvault, group, name)
    taken = keyvault.vaults.check_name_availability({"name": name, "type": "Microsoft.KeyVault/vaults"})
    assert (taken.name_available, taken.reason) == (False, "AlreadyExists")
    invalid = keyvault.vaults.check_name_availability({"name": "1-bad--name", "type": "Microsoft.KeyVault/vaults"})
    assert (invalid.name_available, invalid.reason) == (False, "AccountNameInvalid")
    with pytest.raises(HttpResponseError):
        create(keyvault, group, "x")
    with pytest.raises(HttpResponseError):
        keyvault.vaults.begin_create_or_update(group, unique("kv-"), {"location": "eastus", "properties": {"sku": SKU}}).result()


def test_recover_a_deleted_vault_with_its_contents(credential, keyvault, group):
    name = unique("kv-")
    vault = create(keyvault, group, name)
    SecretClient(vault.properties.vault_uri, credential).set_secret("kept", "value")
    keyvault.vaults.delete(group, name)
    deleted = keyvault.vaults.get_deleted(name, "eastus")
    assert deleted.properties.vault_id.endswith(f"/vaults/{name}")
    assert name in [v.name for v in keyvault.vaults.list_deleted()]
    # The name stays taken while the vault is soft-deleted.
    assert not keyvault.vaults.check_name_availability({"name": name, "type": "Microsoft.KeyVault/vaults"}).name_available
    with pytest.raises(HttpResponseError):
        create(keyvault, group, name)
    recovered = create(keyvault, group, name, createMode="recover")
    assert SecretClient(recovered.properties.vault_uri, credential).get_secret("kept").value == "value"
    with pytest.raises(ResourceNotFoundError):
        keyvault.vaults.get_deleted(name, "eastus")
