"""Managed Identity: user-assigned identities and federated credentials."""


def test_user_assigned_identity_with_federated_credential(emulator, msi, group):
    identity = msi.user_assigned_identities.create_or_update(group, "app-identity", {"location": "eastus"})
    assert identity.tenant_id == emulator.tenant
    assert len(identity.client_id) == 36

    msi.federated_identity_credentials.create_or_update(
        group,
        "app-identity",
        "github",
        {
            "properties": {
                "issuer": "https://token.actions.githubusercontent.com",
                "subject": "repo:octo/app:ref:refs/heads/main",
                "audiences": ["api://AzureADTokenExchange"],
            }
        },
    )
    assert [c.name for c in msi.federated_identity_credentials.list(group, "app-identity")] == ["github"]


def test_identities_on_resources(msi, storage, group):
    from conftest import unique

    app = msi.user_assigned_identities.create_or_update(group, "app", {"location": "eastus"})
    name = unique("st")
    account = storage.storage_accounts.begin_create(
        group,
        name,
        {
            "location": "eastus",
            "kind": "StorageV2",
            "sku": {"name": "Standard_LRS"},
            "identity": {"type": "SystemAssigned,UserAssigned", "userAssignedIdentities": {app.id: {}}},
        },
    ).result()
    assert account.identity.principal_id
    assert account.identity.user_assigned_identities[app.id].client_id == app.client_id

    system = msi.system_assigned_identities.get_by_scope(account.id)
    assert system.principal_id == account.identity.principal_id
    # Listing an identity's associated resources is a preview API the pinned SDK
    # does not include; core/router/tests covers it.


def test_identity_get_update_list_and_delete(msi, group):
    import pytest
    from azure.core.exceptions import ResourceNotFoundError

    created = msi.user_assigned_identities.create_or_update(group, "worker", {"location": "eastus"})
    assert msi.user_assigned_identities.get(group, "worker").principal_id == created.principal_id
    updated = msi.user_assigned_identities.update(group, "worker", {"tags": {"team": "ops"}})
    assert updated.tags == {"team": "ops"} and updated.client_id == created.client_id
    assert [i.name for i in msi.user_assigned_identities.list_by_resource_group(group)] == ["worker"]
    assert created.id in [i.id for i in msi.user_assigned_identities.list_by_subscription()]

    msi.user_assigned_identities.delete(group, "worker")
    with pytest.raises(ResourceNotFoundError):
        msi.user_assigned_identities.get(group, "worker")
    assert list(msi.user_assigned_identities.list_by_resource_group(group)) == []


def test_federated_credential_get_and_delete(msi, group):
    import pytest
    from azure.core.exceptions import ResourceNotFoundError

    msi.user_assigned_identities.create_or_update(group, "deployer", {"location": "eastus"})
    credentials = msi.federated_identity_credentials
    body = {"properties": {"issuer": "https://issuer.example", "subject": "system:serviceaccount:ns:app", "audiences": ["api://AzureADTokenExchange"]}}
    credentials.create_or_update(group, "deployer", "k8s", body)
    got = credentials.get(group, "deployer", "k8s")
    assert (got.issuer, got.subject) == ("https://issuer.example", "system:serviceaccount:ns:app")
    credentials.delete(group, "deployer", "k8s")
    with pytest.raises(ResourceNotFoundError):
        credentials.get(group, "deployer", "k8s")
    assert list(credentials.list(group, "deployer")) == []


def test_listing_the_resources_that_use_an_identity(emulator, arm_request, msi, storage, group):
    from conftest import unique

    app = msi.user_assigned_identities.create_or_update(group, "shared", {"location": "eastus"})
    name = unique("st")
    account = storage.storage_accounts.begin_create(
        group,
        name,
        {
            "location": "eastus",
            "kind": "StorageV2",
            "sku": {"name": "Standard_LRS"},
            "identity": {"type": "UserAssigned", "userAssignedIdentities": {app.id: {}}},
        },
    ).result()
    # A preview API the pinned SDK does not include.
    listed = arm_request("POST", f"{app.id}/listAssociatedResources", "2022-01-31-preview")
    assert listed.status_code == 200, listed.text()
    assert [r["id"].lower() for r in listed.json()["value"]] == [account.id.lower()]
