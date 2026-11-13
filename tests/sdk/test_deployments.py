"""Template deployments through the Resource Manager SDK, with the templates the
Bicep CLI compiled from `core/router/tests/templates/*.bicep`."""

import json
import pathlib

import pytest
from azure.core.exceptions import HttpResponseError

from conftest import unique

TEMPLATES = pathlib.Path(__file__).resolve().parents[2] / "core" / "router" / "tests" / "templates"


def load(name):
    return json.loads((TEMPLATES / f"{name}.json").read_text())


def test_bicep_application(deployments, storage, group):
    poller = deployments.deployments.begin_create_or_update(
        group,
        "main",
        {
            "properties": {
                "mode": "Incremental",
                "template": load("app"),
                "parameters": {"prefix": {"value": "sdk"}, "adminSecret": {"value": "s3cret"}},
            }
        },
    )
    deployment = poller.result()
    assert deployment.properties.provisioning_state == "Succeeded"
    outputs = deployment.properties.outputs
    account = storage.storage_accounts.get_properties(group, outputs["accountName"]["value"])
    assert account.primary_endpoints.blob == outputs["blobEndpoint"]["value"]
    assert len(outputs["containerIds"]["value"]) == 2
    assert deployment.properties.parameters["adminSecret"] == {"type": "SecureString"}

    names = sorted(d.name for d in deployments.deployments.list_by_resource_group(group))
    assert names == ["identity", "main"]
    operations = list(deployments.deployment_operations.list(group, "main"))
    assert operations and all(o.properties.provisioning_state == "Succeeded" for o in operations)
    assert deployments.deployments.check_existence(group, "main")
    assert deployments.deployments.export_template(group, "main").template == load("app")


def test_failed_deployment_raises(deployments, group):
    template = {
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "resources": [{"type": "Microsoft.Unknown/widgets", "apiVersion": "2020-01-01", "name": "w", "location": "eastus"}],
    }
    poller = deployments.deployments.begin_create_or_update(
        group, "broken", {"properties": {"mode": "Incremental", "template": template}}
    )
    with pytest.raises(HttpResponseError):
        poller.result()
    assert deployments.deployments.get(group, "broken").properties.provisioning_state == "Failed"

    with pytest.raises(HttpResponseError) as error:
        deployments.deployments.begin_create_or_update(
            group, "invalid", {"properties": {"mode": "Incremental", "template": {"resources": [], "parameters": {"p": {"type": "string"}}}}}
        ).result()
    assert error.value.error.code == "InvalidTemplate"


def test_validate_and_what_if(deployments, msi, group):
    template = {
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "resources": [
            {"type": "Microsoft.ManagedIdentity/userAssignedIdentities", "apiVersion": "2023-01-31", "name": "planned", "location": "eastus"}
        ],
    }
    body = {"properties": {"mode": "Incremental", "template": template}}
    validated = deployments.deployments.begin_validate(group, "check", body).result()
    assert [r.id.rsplit("/", 1)[1] for r in validated.properties.validated_resources] == ["planned"]

    changes = deployments.deployments.begin_what_if(group, "check", body).result().changes
    assert [(c.resource_id.rsplit("/", 1)[1], c.change_type) for c in changes] == [("planned", "Create")]

    deployments.deployments.begin_create_or_update(group, "check", body).result()
    assert msi.user_assigned_identities.get(group, "planned").client_id
    changes = deployments.deployments.begin_what_if(group, "check", body).result().changes
    assert [c.change_type for c in changes] == ["NoChange"]


def test_subscription_deployment(deployments, resources):
    group = unique("rg-tpl-")
    poller = deployments.deployments.begin_create_or_update_at_subscription_scope(
        unique("sub-"),
        {
            "location": "eastus",
            "properties": {"mode": "Incremental", "template": load("subscription"), "parameters": {"groupName": {"value": group}}},
        },
    )
    try:
        deployment = poller.result()
        assert deployment.properties.provisioning_state == "Succeeded"
        assert resources.resource_groups.get(group).tags == {"owner": "templates"}
    finally:
        if resources.resource_groups.check_existence(group):
            resources.resource_groups.begin_delete(group).result()
