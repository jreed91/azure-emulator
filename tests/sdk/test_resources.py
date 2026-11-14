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


def test_subscriptions_and_tenants(emulator, arm_request):
    import pytest

    listed = arm_request("GET", "/subscriptions", "2022-12-01").json()["value"]
    assert [(s["subscriptionId"], s["state"], s["tenantId"]) for s in listed] == [(emulator.subscription, "Enabled", emulator.tenant)]
    one = arm_request("GET", f"/subscriptions/{emulator.subscription}", "2022-12-01")
    assert one.status_code == 200 and one.json()["id"] == f"/subscriptions/{emulator.subscription}"
    missing = arm_request("GET", "/subscriptions/99999999-9999-9999-9999-999999999999", "2022-12-01")
    assert missing.status_code == 404 and missing.json()["error"]["code"] == "SubscriptionNotFound"
    tenants = arm_request("GET", "/tenants", "2022-12-01").json()["value"]
    assert [t["tenantId"] for t in tenants] == [emulator.tenant]


def test_providers(resources):
    storage = resources.providers.get("Microsoft.Storage")
    assert storage.registration_state == "Registered"
    assert "storageAccounts" in [t.resource_type for t in storage.resource_types]
    namespaces = [p.namespace for p in resources.providers.list()]
    assert {"Microsoft.Storage", "Microsoft.KeyVault", "Microsoft.ServiceBus"} <= set(namespaces)


def test_listing_resources_across_the_subscription(resources, msi, group):
    identity = msi.user_assigned_identities.create_or_update(group, "listed", {"location": "eastus"})
    everything = [r.id for r in resources.resources.list()]
    assert identity.id in everything
    filtered = [r.id for r in resources.resources.list(filter="resourceType eq 'Microsoft.ManagedIdentity/userAssignedIdentities'")]
    assert identity.id in filtered and all("userAssignedIdentities" in i for i in filtered)


def test_resource_group_update_and_location_conflict(resources, group):
    import pytest
    from azure.core.exceptions import HttpResponseError

    updated = resources.resource_groups.update(group, {"tags": {"cost-center": "42"}})
    assert updated.tags == {"cost-center": "42"}
    assert resources.resource_groups.get(group).tags == {"cost-center": "42"}
    with pytest.raises(HttpResponseError) as refused:
        resources.resource_groups.create_or_update(group, {"location": "westus"})
    assert refused.value.status_code == 409 and refused.value.error.code == "InvalidResourceGroupLocation"


def test_request_refusals(emulator, arm_request, credential):
    import requests

    groups = f"/subscriptions/{emulator.subscription}/resourcegroups"
    # api-version is required, and must look like one.
    assert arm_request("GET", groups, "2021-04-01").status_code == 200
    no_version = requests.get(f"{emulator.base}{groups}", headers={"Authorization": f"Bearer {credential.get_token(emulator.base + '/.default').token}"}, verify=emulator.ca)
    assert no_version.status_code == 400 and no_version.json()["error"]["code"] == "MissingApiVersionParameter"
    bad_version = arm_request("GET", groups, "yesterday")
    assert bad_version.status_code == 400 and bad_version.json()["error"]["code"] == "InvalidApiVersionParameter"

    # A token for another audience, and no token at all.
    other = credential.get_token("https://vault.azure.net/.default").token
    wrong = requests.get(f"{emulator.base}{groups}?api-version=2021-04-01", headers={"Authorization": f"Bearer {other}"}, verify=emulator.ca)
    assert wrong.status_code == 401 and wrong.json()["error"]["code"] == "InvalidAuthenticationTokenAudience"
    assert "Bearer authorization_uri=" in wrong.headers["WWW-Authenticate"]
    anonymous = requests.get(f"{emulator.base}{groups}?api-version=2021-04-01", verify=emulator.ca)
    assert anonymous.status_code == 401

    # A body that is not JSON.
    garbage = requests.put(
        f"{emulator.base}{groups}/bad-body?api-version=2021-04-01",
        data=b"{not json",
        headers={"Authorization": f"Bearer {credential.get_token(emulator.base + '/.default').token}", "Content-Type": "application/json"},
        verify=emulator.ca,
    )
    assert garbage.status_code == 400 and garbage.json()["error"]["code"] == "InvalidRequestContent"


def test_older_cloud_metadata_format(emulator):
    import requests

    # Before 2020-06-01 the document is a single object with the older names.
    old = requests.get(f"{emulator.base}/metadata/endpoints?api-version=2019-05-01", verify=emulator.ca).json()
    assert old["authentication"]["loginEndpoint"] == f"{emulator.base}/"
    new = requests.get(f"{emulator.base}/metadata/endpoints?api-version=2022-09-01", verify=emulator.ca).json()
    assert new["resourceManager"] == f"{emulator.base}/"
