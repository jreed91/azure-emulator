"""Key Vault: vaults on Resource Manager and the secrets data plane."""

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.keyvault.secrets import SecretClient

from conftest import TENANT, unique


def test_vault_uri(emulator, vault):
    assert vault.properties.vault_uri == f"https://{vault.name}.vault.azure.localhost:{emulator.port}/"


@pytest.mark.live
def test_secret_lifecycle(credential, vault):
    secrets = SecretClient(vault.properties.vault_uri, credential)
    first = secrets.set_secret("api-key", "v1", content_type="text/plain")
    secrets.set_secret("api-key", "v2")
    assert secrets.get_secret("api-key").value == "v2"
    assert secrets.get_secret("api-key", first.properties.version).value == "v1"
    assert [s.name for s in secrets.list_properties_of_secrets()] == ["api-key"]
    assert len(list(secrets.list_properties_of_secret_versions("api-key"))) == 2

    secrets.update_secret_properties("api-key", tags={"rotated": "yes"})
    assert secrets.get_secret("api-key").properties.tags == {"rotated": "yes"}

    assert secrets.begin_delete_secret("api-key").result().name == "api-key"
    with pytest.raises(ResourceNotFoundError):
        secrets.get_secret("api-key")
    secrets.begin_recover_deleted_secret("api-key").result()
    assert secrets.get_secret("api-key").value == "v2"
    secrets.begin_delete_secret("api-key").wait()
    secrets.purge_deleted_secret("api-key")
    with pytest.raises(ResourceNotFoundError):
        secrets.get_deleted_secret("api-key")


@pytest.mark.live
def test_secret_backup_and_restore(credential, vault):
    secrets = SecretClient(vault.properties.vault_uri, credential)
    first = secrets.set_secret("portable", "v1", content_type="text/plain")
    second = secrets.set_secret("portable", "v2", tags={"env": "test"})
    backup = secrets.backup_secret("portable")
    assert isinstance(backup, bytes) and backup

    with pytest.raises(HttpResponseError) as conflict:
        secrets.restore_secret_backup(backup)
    assert conflict.value.status_code == 409, "a live secret's name is taken"
    secrets.begin_delete_secret("portable").wait()
    with pytest.raises(HttpResponseError) as conflict:
        secrets.restore_secret_backup(backup)
    assert conflict.value.status_code == 409, "a deleted secret's name is still taken"
    secrets.purge_deleted_secret("portable")
    with pytest.raises(ResourceNotFoundError):
        secrets.get_secret("portable")

    restored = secrets.restore_secret_backup(backup)
    assert (restored.name, restored.version, restored.id) == ("portable", second.properties.version, second.id)
    assert restored.tags == {"env": "test"}
    current = secrets.get_secret("portable")
    assert (current.value, current.properties.version) == ("v2", second.properties.version)
    older = secrets.get_secret("portable", first.properties.version)
    assert (older.value, older.properties.content_type) == ("v1", "text/plain")
    versions = {v.version for v in secrets.list_properties_of_secret_versions("portable")}
    assert versions == {first.properties.version, second.properties.version}

    with pytest.raises(ResourceNotFoundError):
        secrets.backup_secret("never-created")
    with pytest.raises(HttpResponseError) as corrupt:
        secrets.restore_secret_backup(b"not a backup")
    assert corrupt.value.status_code == 400


def test_access_policy_update(emulator, keyvault, group, vault):
    policy = {"tenantId": emulator.tenant, "objectId": "00000000-0000-0000-0000-000000000001", "permissions": {"secrets": ["get"]}}
    keyvault.vaults.update_access_policy(group, vault.name, "add", {"properties": {"accessPolicies": [policy]}})
    assert keyvault.vaults.get(group, vault.name).properties.access_policies[0].permissions.secrets == ["get"]


def test_secrets_through_resource_manager(credential, keyvault, group, vault):
    keyvault.secrets.create_or_update(group, vault.name, "from-arm", {"properties": {"value": "set through resource manager"}})
    assert SecretClient(vault.properties.vault_uri, credential).get_secret("from-arm").value == "set through resource manager"
    assert [s.name for s in keyvault.secrets.list(group, vault.name)] == ["from-arm"]


def test_vault_soft_delete_and_purge(keyvault, group, vault):
    keyvault.vaults.delete(group, vault.name)
    assert vault.name in [v.name for v in keyvault.vaults.list_deleted()]
    assert keyvault.vaults.get_deleted(vault.name, "eastus").properties.vault_id.endswith(f"/vaults/{vault.name}")
    keyvault.vaults.begin_purge_deleted(vault.name, "eastus").result()
    assert vault.name not in [v.name for v in keyvault.vaults.list_deleted()]


def test_managed_hsm_soft_delete_and_purge(emulator, keyvault, group):
    from conftest import unique

    name = unique("hsm-")
    hsm = keyvault.managed_hsms.begin_create_or_update(
        group,
        name,
        {"location": "eastus", "properties": {"tenantId": emulator.tenant, "initialAdminObjectIds": ["00000000-0000-0000-0000-0000000000aa"]}},
    ).result()
    assert hsm.properties.hsm_uri == f"https://{name}.managedhsm.azure.localhost:{emulator.port}/"
    assert hsm.sku.name == "Standard_B1"
    assert not keyvault.managed_hsms.check_mhsm_name_availability({"name": name}).name_available

    keyvault.managed_hsms.begin_delete(group, name).result()
    assert name in [h.name for h in keyvault.managed_hsms.list_deleted()]
    assert keyvault.managed_hsms.get_deleted(name, "eastus").properties.mhsm_id.endswith(f"/managedHSMs/{name}")
    keyvault.managed_hsms.begin_purge_deleted(name, "eastus").result()
    assert keyvault.managed_hsms.check_mhsm_name_availability({"name": name}).name_available


def test_no_private_endpoint_connections_without_microsoft_network(keyvault, group, vault):
    assert list(keyvault.private_endpoint_connections.list_by_resource(group, vault.name)) == []


def test_keys_through_resource_manager(keyvault, group, vault):
    created = keyvault.keys.create_if_not_exist(group, vault.name, "signing", {"properties": {"kty": "EC", "curveName": "P-384"}})
    assert created.curve_name == "P-384" and created.key_ops == ["sign", "verify"]
    again = keyvault.keys.create_if_not_exist(group, vault.name, "signing", {"properties": {"kty": "EC"}})
    assert again.key_uri_with_version == created.key_uri_with_version
    assert [k.name for k in keyvault.keys.list(group, vault.name)] == ["signing"]
    assert len(list(keyvault.keys.list_versions(group, vault.name, "signing"))) == 1
    version = created.key_uri_with_version.rsplit("/", 1)[1]
    assert keyvault.keys.get_version(group, vault.name, "signing", version).kty == "EC"


def test_purge_protection_blocks_a_purge(keyvault, group):
    name = unique("kv-")
    keyvault.vaults.begin_create_or_update(
        group,
        name,
        {
            "location": "eastus",
            "properties": {
                "tenantId": TENANT,
                "sku": {"family": "A", "name": "standard"},
                "accessPolicies": [],
                "enableSoftDelete": True,
                "enablePurgeProtection": True,
            },
        },
    ).result()
    keyvault.vaults.delete(group, name)
    with pytest.raises(HttpResponseError):
        keyvault.vaults.begin_purge_deleted(name, "eastus").result()
    assert name in [v.name for v in keyvault.vaults.list_deleted()]


def protected_vault(keyvault, group):
    name = unique("kv-")
    properties = {
        "tenantId": TENANT,
        "sku": {"family": "A", "name": "standard"},
        "accessPolicies": [],
        "enableSoftDelete": True,
        "enablePurgeProtection": True,
    }
    return keyvault.vaults.begin_create_or_update(group, name, {"location": "eastus", "properties": properties}).result()


def test_purge_protection_cannot_be_turned_off(keyvault, group):
    vault = protected_vault(keyvault, group)
    properties = {
        "tenantId": TENANT,
        "sku": {"family": "A", "name": "standard"},
        "accessPolicies": [],
        "enablePurgeProtection": False,
    }
    with pytest.raises(HttpResponseError) as refused:
        keyvault.vaults.begin_create_or_update(group, vault.name, {"location": "eastus", "properties": properties}).result()
    assert refused.value.status_code == 400
    assert keyvault.vaults.get(group, vault.name).properties.enable_purge_protection is True


def test_purge_protection_blocks_purging_a_secret(keyvault, group, credential):
    vault = protected_vault(keyvault, group)
    secrets = SecretClient(vault.properties.vault_uri, credential)
    secrets.set_secret("kept", "value")
    secrets.begin_delete_secret("kept").wait()
    with pytest.raises(HttpResponseError) as refused:
        secrets.purge_deleted_secret("kept")
    assert refused.value.status_code == 403
    # Still there to recover.
    secrets.begin_recover_deleted_secret("kept").result()
    assert secrets.get_secret("kept").value == "value"
