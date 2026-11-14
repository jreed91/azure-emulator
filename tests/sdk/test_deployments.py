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


FUNCTIONS_TEMPLATE = {
    "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
    "contentVersion": "1.0.0.0",
    "functions": [
        {
            "namespace": "app",
            "members": {
                "greet": {
                    "parameters": [{"name": "name", "type": "string"}],
                    "output": {"type": "string", "value": "[concat('Hello, ', parameters('name'))]"},
                }
            },
        }
    ],
    "parameters": {
        "account": {"type": "string"},
        "stamp": {"type": "string", "defaultValue": "[utcNow()]"},
    },
    "variables": {
        "nums": [3, 1, 2],
        "words": ["apple", "avocado", "banana"],
        "base": "2024-01-02T03:04:05Z",
    },
    "resources": [
        {
            "type": "Microsoft.Storage/storageAccounts",
            "apiVersion": "2023-01-01",
            "name": "[parameters('account')]",
            "location": "eastus",
            "kind": "StorageV2",
            "sku": {"name": "Standard_LRS"},
            "properties": {
                "networkAcls": {
                    "defaultAction": "Allow",
                    "copy": [{"name": "ipRules", "count": 2, "input": {"value": "[format('10.0.{0}.0/24', copyIndex('ipRules'))]", "action": "Allow"}}],
                }
            },
        }
    ],
    "outputs": {
        "numbers": {"type": "array", "value": "[createArray(format('{0:N2}', json('1234.5')), format('{0:D5}', 42), format('{0:X}', 255), format('{0:x4}', 255), format('{0:N}', 1234), format('{0:F1}', json('3.14159')))]"},
        "times": {
            "type": "array",
            "value": "[createArray(dateTimeAdd(variables('base'), 'PT1H', 'o'), dateTimeAdd(variables('base'), 'PT1H', 'u'), dateTimeAdd(variables('base'), 'PT1H', 's'), dateTimeAdd(variables('base'), 'PT1H', 'd'), dateTimeAdd(variables('base'), 'PT10H', 'hh:mm tt'))]",
        },
        "filtered": {"type": "array", "value": "[filter(variables('nums'), lambda('x', greater(lambdaVariables('x'), 1)))]"},
        "mapped": {"type": "array", "value": "[map(variables('nums'), lambda('x', mul(lambdaVariables('x'), 10)))]"},
        "grouped": {"type": "object", "value": "[groupBy(variables('words'), lambda('w', first(lambdaVariables('w'))))]"},
        "mappedValues": {"type": "object", "value": "[mapValues(createObject('a', 1, 'b', 2), lambda('v', add(lambdaVariables('v'), 1)))]"},
        "comparisons": {"type": "array", "value": "[createArray(less(1, 2), lessOrEquals(2, 2), greater('b', 'a'), or(false, true))]"},
        "arithmetic": {"type": "array", "value": "[createArray(add(2, 3), sub(5, 8), mul(4, 5), div(7, 2), mod(7, 2), min(variables('nums')), max(3, 9))]"},
        "conversions": {"type": "array", "value": "[createArray(bool('TRUE'), string(true), string(false))]"},
        "greeting": {"type": "string", "value": "[app.greet('Ada')]"},
        "search": {"type": "array", "value": "[createArray(startsWith('abcdef', 'AB'), endsWith('abcdef', 'ef'), indexOf('abcabc', 'bc'), lastIndexOf('abcabc', 'bc'), indexOf('abc', ''), lastIndexOf('abc', ''))]"},
        "slices": {"type": "array", "value": "[createArray(first('hello'), last(createArray(1, 2, 3)), skip('hello', 3), take(createArray(1, 2, 3), 2))]"},
        "tenantId": {"type": "string", "value": "[tenant().tenantId]"},
        "stamp": {"type": "string", "value": "[parameters('stamp')]"},
        "ipRule": {"type": "string", "value": "[reference(parameters('account')).networkAcls.ipRules[1].value]"},
    },
}


def test_template_functions(emulator, deployments, group):
    import re

    deployment = deployments.deployments.begin_create_or_update(
        group,
        "functions",
        {"properties": {"mode": "Incremental", "template": FUNCTIONS_TEMPLATE, "parameters": {"account": {"value": unique("fn")}}}},
    ).result()
    out = {name: value["value"] for name, value in deployment.properties.outputs.items()}
    assert out["numbers"] == ["1,234.50", "00042", "FF", "00ff", "1,234.00", "3.1"]
    assert out["times"] == ["2024-01-02T04:04:05.0000000Z", "2024-01-02 04:04:05Z", "2024-01-02T04:04:05", "1/2/2024", "01:04 PM"]
    assert out["filtered"] == [3, 2]
    assert out["mapped"] == [30, 10, 20]
    assert out["grouped"] == {"a": ["apple", "avocado"], "b": ["banana"]}
    assert out["mappedValues"] == {"a": 2, "b": 3}
    assert out["comparisons"] == [True, True, True, True]
    assert out["arithmetic"] == [5, -3, 20, 3, 1, 1, 9]
    assert out["conversions"] == [True, "True", "False"]
    assert out["greeting"] == "Hello, Ada"
    assert out["search"] == [True, True, 1, 4, 0, 2]
    assert out["slices"] == ["h", 3, "lo", [1, 2]]
    assert out["tenantId"] == emulator.tenant
    assert re.fullmatch(r"\d{8}T\d{6}Z", out["stamp"])
    assert out["ipRule"] == "10.0.1.0/24"


def test_calculate_template_hash(deployments):
    first = deployments.deployments.calculate_template_hash(FUNCTIONS_TEMPLATE)
    again = deployments.deployments.calculate_template_hash(FUNCTIONS_TEMPLATE)
    changed = deployments.deployments.calculate_template_hash({**FUNCTIONS_TEMPLATE, "contentVersion": "2.0.0.0"})
    assert first.template_hash == again.template_hash != changed.template_hash
    assert first.minified_template


def test_complete_mode_deletes_what_the_template_does_not_list(deployments, msi, group):
    msi.user_assigned_identities.create_or_update(group, "stray", {"location": "eastus"})
    template = {
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "resources": [{"type": "Microsoft.ManagedIdentity/userAssignedIdentities", "apiVersion": "2023-01-31", "name": "kept", "location": "eastus"}],
    }
    deployments.deployments.begin_create_or_update(group, "complete", {"properties": {"mode": "Complete", "template": template}}).result()
    assert [i.name for i in msi.user_assigned_identities.list_by_resource_group(group)] == ["kept"]


def test_key_vault_reference_in_parameters(credential, deployments, keyvault, group, vault):
    from azure.keyvault.secrets import SecretClient

    # Only a vault that allows it lends secrets to deployments.
    vault = keyvault.vaults.begin_create_or_update(
        group,
        vault.name,
        {"location": vault.location, "properties": {"tenantId": vault.properties.tenant_id, "sku": {"family": "A", "name": "standard"}, "accessPolicies": [], "enabledForTemplateDeployment": True}},
    ).result()

    SecretClient(vault.properties.vault_uri, credential).set_secret("admin-password", "from-the-vault")
    template = {
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "parameters": {"password": {"type": "securestring"}},
        "resources": [],
        # Echoing a secure parameter is only for the test's sake.
        "outputs": {"length": {"type": "int", "value": "[length(parameters('password'))]"}},
    }
    parameters = {"password": {"reference": {"keyVault": {"id": vault.id}, "secretName": "admin-password"}}}
    deployment = deployments.deployments.begin_create_or_update(
        group, "kv-reference", {"properties": {"mode": "Incremental", "template": template, "parameters": parameters}}
    ).result()
    assert deployment.properties.outputs["length"]["value"] == len("from-the-vault")


def test_nested_deployment_with_outer_scope(deployments, msi, group):
    template = {
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "variables": {"identityName": "from-outer-scope"},
        "resources": [
            {
                "type": "Microsoft.Resources/deployments",
                "apiVersion": "2022-09-01",
                "name": "inner",
                "properties": {
                    "mode": "Incremental",
                    # Outer scope: the nested template sees the parent's variables.
                    "expressionEvaluationOptions": {"scope": "outer"},
                    "template": {
                        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
                        "contentVersion": "1.0.0.0",
                        "resources": [{"type": "Microsoft.ManagedIdentity/userAssignedIdentities", "apiVersion": "2023-01-31", "name": "[variables('identityName')]", "location": "eastus"}],
                    },
                },
            }
        ],
    }
    deployments.deployments.begin_create_or_update(group, "outer", {"properties": {"mode": "Incremental", "template": template}}).result()
    assert msi.user_assigned_identities.get(group, "from-outer-scope").name == "from-outer-scope"
