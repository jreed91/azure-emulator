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
