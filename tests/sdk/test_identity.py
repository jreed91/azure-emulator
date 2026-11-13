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
