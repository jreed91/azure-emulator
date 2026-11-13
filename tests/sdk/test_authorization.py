"""Role definitions and role assignments through the authorization SDK."""

import uuid

from conftest import SUBSCRIPTION

READER = "acdd72a7-3385-48ef-bd42-f606fba81ae7"


def test_built_in_roles(authorization):
    scope = f"/subscriptions/{SUBSCRIPTION}"
    owner = authorization.role_definitions.get(scope, "8e3af657-a8ff-443c-a75c-2fe8c4bcb635")
    assert owner.role_name == "Owner"
    assert owner.permissions[0].actions == ["*"]
    found = list(authorization.role_definitions.list(scope, filter="roleName eq 'Key Vault Secrets User'"))
    assert [r.name for r in found] == ["4633458b-17de-408a-b874-0445c86b69e6"]


def test_role_assignment_lifecycle(authorization, msi, group):
    identity = msi.user_assigned_identities.create_or_update(group, "app-id", {"location": "eastus"})
    scope = f"/subscriptions/{SUBSCRIPTION}/resourceGroups/{group}"
    name = str(uuid.uuid4())
    created = authorization.role_assignments.create(
        scope,
        name,
        {
            "role_definition_id": f"/subscriptions/{SUBSCRIPTION}/providers/Microsoft.Authorization/roleDefinitions/{READER}",
            "principal_id": identity.principal_id,
        },
    )
    assert created.principal_type == "ServicePrincipal"
    assert created.scope == scope
    assert authorization.role_assignments.get(scope, name).principal_id == identity.principal_id
    listed = list(authorization.role_assignments.list_for_resource_group(group, filter=f"principalId eq '{identity.principal_id}'"))
    assert [a.name for a in listed] == [name]
    assert authorization.role_assignments.get_by_id(created.id).name == name
    authorization.role_assignments.delete(scope, name)
    assert not list(authorization.role_assignments.list_for_scope(scope, filter="atScope()"))
