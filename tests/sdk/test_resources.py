"""Resource Manager itself: resource groups and their lifecycle."""

from conftest import unique


def test_resource_group_lifecycle(resources):
    name = unique("rg-")
    group = resources.resource_groups.create_or_update(name, {"location": "eastus", "tags": {"by": "sdk-tests"}})
    assert group.name == name
    assert group.properties.provisioning_state == "Succeeded"
    assert group.tags == {"by": "sdk-tests"}
    assert resources.resource_groups.check_existence(name)
    assert name in [g.name for g in resources.resource_groups.list()]

    resources.resource_groups.begin_delete(name).result()
    assert not resources.resource_groups.check_existence(name)


def test_deleting_a_group_deletes_what_is_in_it(resources, storage, group, account):
    resources.resource_groups.begin_delete(group).result()
    assert not resources.resource_groups.check_existence(group)
    assert storage.storage_accounts.check_name_availability({"name": account.name}).name_available
