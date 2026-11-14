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


def vault_path(emulator, group, name, kind="vaults"):
    return f"/subscriptions/{emulator.subscription}/resourceGroups/{group}/providers/Microsoft.KeyVault/{kind}/{name}"


@pytest.fixture
def hsm(emulator, keyvault, group):
    return keyvault.managed_hsms.begin_create_or_update(
        group,
        unique("hsm-"),
        {"location": "eastus", "properties": {"tenantId": emulator.tenant, "initialAdminObjectIds": ["00000000-0000-0000-0000-0000000000aa"]}},
    ).result()


def test_managed_hsm_get_update_and_list(keyvault, group, hsm):
    got = keyvault.managed_hsms.get(group, hsm.name)
    assert (got.name, got.location, got.properties.provisioning_state) == (hsm.name, "eastus", "Succeeded")
    updated = keyvault.managed_hsms.begin_update(group, hsm.name, {"tags": {"env": "test"}}).result()
    assert updated.tags == {"env": "test"}
    assert keyvault.managed_hsms.get(group, hsm.name).tags == {"env": "test"}
    assert [h.name for h in keyvault.managed_hsms.list_by_resource_group(group)] == [hsm.name]
    assert hsm.name in [h.name for h in keyvault.managed_hsms.list_by_subscription()]


def test_managed_hsm_regions_and_private_link_resources(keyvault, group, hsm):
    [region] = keyvault.mhsm_regions.list_by_resource(group, hsm.name)
    assert (region.name, region.is_primary) == ("eastus", True)
    [resource] = keyvault.mhsm_private_link_resources.list_by_mhsm_resource(group, hsm.name).value
    assert resource.group_id == "managedhsm"


def test_managed_hsm_keys_through_resource_manager(keyvault, group, hsm):
    keys = keyvault.managed_hsm_keys
    created = keys.create_if_not_exist(group, hsm.name, "wrapping", {"properties": {"kty": "RSA-HSM", "keySize": 3072}})
    assert (created.kty, created.key_size) == ("RSA-HSM", 3072)
    again = keys.create_if_not_exist(group, hsm.name, "wrapping", {"properties": {"kty": "RSA-HSM"}})
    assert again.key_uri_with_version == created.key_uri_with_version
    assert keys.get(group, hsm.name, "wrapping").key_uri_with_version == created.key_uri_with_version
    assert [k.name for k in keys.list(group, hsm.name)] == ["wrapping"]
    [version] = keys.list_versions(group, hsm.name, "wrapping")
    assert keys.get_version(group, hsm.name, "wrapping", version.name).kty == "RSA-HSM"
    with pytest.raises(ResourceNotFoundError):
        keys.get(group, hsm.name, "missing")


@pytest.mark.parametrize("kind", ["vaults", "managedHSMs"])
def test_private_endpoint_connections_are_absent(emulator, arm_request, keyvault, group, vault, hsm, kind):
    """Microsoft.Network creates these; without it there are none."""
    name = vault.name if kind == "vaults" else hsm.name
    operations = keyvault.private_endpoint_connections if kind == "vaults" else keyvault.mhsm_private_endpoint_connections
    listing = operations.list_by_resource(group, name)
    assert list(listing) == []
    with pytest.raises(ResourceNotFoundError):
        operations.get(group, name, "connection")
    with pytest.raises(HttpResponseError) as refused:
        operations.put(group, name, "connection", {"properties": {"privateLinkServiceConnectionState": {"status": "Approved"}}})
    assert refused.value.status_code == 404
    deleted = arm_request("DELETE", f"{vault_path(emulator, group, name, kind)}/privateEndpointConnections/connection", "2024-11-01")
    assert deleted.status_code in (204, 404)


def test_vault_private_link_resources(keyvault, group, vault):
    [resource] = keyvault.private_link_resources.list_by_vault(group, vault.name).value
    assert (resource.group_id, resource.required_zone_names) == ("vault", ["privatelink.vaultcore.azure.net"])


def test_vault_network_security_perimeters_are_absent(emulator, arm_request, group, vault):
    base = f"{vault_path(emulator, group, vault.name)}/networkSecurityPerimeterConfigurations"
    listed = arm_request("GET", base, "2024-12-01-preview")
    assert listed.status_code == 200 and listed.json()["value"] == [], listed.text()
    assert arm_request("GET", f"{base}/association", "2024-12-01-preview").status_code == 404
    assert arm_request("POST", f"{base}/association/reconcile", "2024-12-01-preview", {}).status_code == 404


def test_secret_get_and_update_through_resource_manager(keyvault, group, vault):
    keyvault.secrets.create_or_update(group, vault.name, "db-password", {"properties": {"value": "hunter2", "contentType": "text/plain"}})
    got = keyvault.secrets.get(group, vault.name, "db-password")
    # Resource Manager never returns a secret's value.
    assert (got.name, got.properties.content_type, got.properties.value) == ("db-password", "text/plain", None)
    updated = keyvault.secrets.update(group, vault.name, "db-password", {"tags": {"rotated": "yes"}, "properties": {"contentType": "application/x-pem"}})
    assert updated.tags == {"rotated": "yes"} and updated.properties.content_type == "application/x-pem"
    assert keyvault.secrets.get(group, vault.name, "db-password").properties.content_type == "application/x-pem"
    with pytest.raises(ResourceNotFoundError):
        keyvault.secrets.get(group, vault.name, "missing")
