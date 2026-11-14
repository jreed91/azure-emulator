"""The Service Bus control plane beyond the basics: namespace settings and
names, authorization rules and keys at every level, network rule sets,
geo-disaster-recovery aliases and Standard-to-Premium migration.

Expected values follow the Service Bus management REST reference."""

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError

from conftest import unique

PREMIUM = {"location": "eastus", "sku": {"name": "Premium"}}
STANDARD = {"location": "eastus", "sku": {"name": "Standard"}}


def create(servicebus, group, body, prefix="sb-"):
    return servicebus.namespaces.begin_create_or_update(group, unique(prefix), body).result()


@pytest.mark.parametrize(
    "name, available, reason",
    [
        ("short", False, "InvalidName"),  # under 6 characters
        ("1starts-with-digit", False, "InvalidName"),
        ("ends-with-hyphen-", False, "InvalidName"),
        ("has_underscore", False, "InvalidName"),
        ("x" * 51, False, "InvalidName"),
        ("x" * 50, True, "None"),
        ("valid-name-123", True, "None"),
    ],
)
def test_namespace_name_rules(servicebus, name, available, reason):
    result = servicebus.namespaces.check_name_availability({"name": name})
    assert (result.name_available, result.reason) == (available, reason)


def test_namespace_defaults_update_and_refusals(servicebus, group):
    namespace = create(servicebus, group, STANDARD)
    assert (namespace.sku.name, namespace.sku.tier) == ("Standard", "Standard")
    assert namespace.status == "Active" and namespace.provisioning_state == "Succeeded"
    assert (namespace.minimum_tls_version, namespace.public_network_access, namespace.disable_local_auth) == ("1.2", "Enabled", False)
    taken = servicebus.namespaces.check_name_availability({"name": namespace.name})
    assert (taken.name_available, taken.reason) == (False, "NameInUse")

    updated = servicebus.namespaces.update(group, namespace.name, {"tags": {"env": "test"}, "properties": {"disableLocalAuth": True}})
    assert updated.tags == {"env": "test"} and updated.disable_local_auth is True
    assert updated.created_at == namespace.created_at

    premium = create(servicebus, group, PREMIUM)
    assert premium.sku.capacity == 1, "a Premium namespace starts with one messaging unit"

    names = {n.name for n in servicebus.namespaces.list_by_resource_group(group)}
    assert {namespace.name, premium.name} <= names
    assert {namespace.name, premium.name} <= {n.name for n in servicebus.namespaces.list()}

    with pytest.raises(HttpResponseError) as refused:
        servicebus.namespaces.begin_create_or_update(group, unique("sb-"), {"location": "eastus", "sku": {"name": "Gold"}}).result()
    assert refused.value.status_code == 400
    with pytest.raises(HttpResponseError) as refused:
        servicebus.namespaces.begin_create_or_update(group, "bad_name", STANDARD).result()
    assert refused.value.status_code == 400


def test_namespace_name_is_global(servicebus, resources, group):
    namespace = create(servicebus, group, STANDARD)
    other = unique("rg-")
    resources.resource_groups.create_or_update(other, {"location": "eastus"})
    try:
        with pytest.raises(HttpResponseError) as refused:
            servicebus.namespaces.begin_create_or_update(other, namespace.name, STANDARD).result()
        assert refused.value.status_code == 409
    finally:
        resources.resource_groups.begin_delete(other).result()
    servicebus.namespaces.begin_delete(group, namespace.name).result()
    with pytest.raises(ResourceNotFoundError):
        servicebus.namespaces.get(group, namespace.name)


@pytest.mark.parametrize(
    "rights, ok",
    [
        (["Listen"], True),
        (["Send"], True),
        (["Listen", "Send"], True),
        (["Manage", "Listen", "Send"], True),
        (["Manage"], False),  # Manage needs Send and Listen
        (["Manage", "Send"], False),
        ([], False),
        (["Read"], False),
    ],
)
def test_authorization_rule_rights(servicebus, group, rights, ok):
    namespace = create(servicebus, group, STANDARD)
    if ok:
        rule = servicebus.namespaces.create_or_update_authorization_rule(group, namespace.name, "app", {"properties": {"rights": rights}})
        assert sorted(rule.rights) == sorted(rights)
    else:
        with pytest.raises(HttpResponseError) as refused:
            servicebus.namespaces.create_or_update_authorization_rule(group, namespace.name, "app", {"properties": {"rights": rights}})
        assert refused.value.status_code == 400


def test_rules_and_keys_at_every_level(servicebus, group):
    namespace = create(servicebus, group, STANDARD)
    ns = namespace.name
    servicebus.queues.create_or_update(group, ns, "jobs", {})
    servicebus.topics.create_or_update(group, ns, "events", {})

    levels = [
        (
            lambda name, body: servicebus.namespaces.create_or_update_authorization_rule(group, ns, name, body),
            lambda name: servicebus.namespaces.list_keys(group, ns, name),
            lambda name, kind: servicebus.namespaces.regenerate_keys(group, ns, name, {"keyType": kind}),
            lambda: [r.name for r in servicebus.namespaces.list_authorization_rules(group, ns)],
            lambda name: servicebus.namespaces.delete_authorization_rule(group, ns, name),
            "",
        ),
        (
            lambda name, body: servicebus.queues.create_or_update_authorization_rule(group, ns, "jobs", name, body),
            lambda name: servicebus.queues.list_keys(group, ns, "jobs", name),
            lambda name, kind: servicebus.queues.regenerate_keys(group, ns, "jobs", name, {"keyType": kind}),
            lambda: [r.name for r in servicebus.queues.list_authorization_rules(group, ns, "jobs")],
            lambda name: servicebus.queues.delete_authorization_rule(group, ns, "jobs", name),
            ";EntityPath=jobs",
        ),
        (
            lambda name, body: servicebus.topics.create_or_update_authorization_rule(group, ns, "events", name, body),
            lambda name: servicebus.topics.list_keys(group, ns, "events", name),
            lambda name, kind: servicebus.topics.regenerate_keys(group, ns, "events", name, {"keyType": kind}),
            lambda: [r.name for r in servicebus.topics.list_authorization_rules(group, ns, "events")],
            lambda name: servicebus.topics.delete_authorization_rule(group, ns, "events", name),
            ";EntityPath=events",
        ),
    ]
    for put, keys_of, regenerate, names, delete, entity_path in levels:
        put("sender", {"properties": {"rights": ["Send"]}})
        assert "sender" in names()
        keys = keys_of("sender")
        assert keys.key_name == "sender" and keys.primary_key != keys.secondary_key
        assert keys.primary_connection_string.endswith(f"SharedAccessKey={keys.primary_key}{entity_path}") or (
            f"SharedAccessKey={keys.primary_key}" in keys.primary_connection_string and entity_path in keys.primary_connection_string
        )

        fresh = regenerate("sender", "PrimaryKey")
        assert fresh.primary_key != keys.primary_key and fresh.secondary_key == keys.secondary_key
        again = regenerate("sender", "SecondaryKey")
        assert again.primary_key == fresh.primary_key and again.secondary_key != keys.secondary_key
        assert keys_of("sender").primary_key == fresh.primary_key

        with pytest.raises(HttpResponseError) as refused:
            regenerate("sender", "TertiaryKey")
        assert refused.value.status_code == 400

        delete("sender")
        assert "sender" not in names()
        with pytest.raises(ResourceNotFoundError):
            keys_of("sender")


def test_network_rule_set(servicebus, group):
    namespace = create(servicebus, group, PREMIUM)
    default = servicebus.namespaces.get_network_rule_set(group, namespace.name)
    assert (default.name, default.default_action, default.ip_rules, default.virtual_network_rules) == ("default", "Allow", [], [])

    updated = servicebus.namespaces.create_or_update_network_rule_set(
        group,
        namespace.name,
        {
            "properties": {
                "defaultAction": "Deny",
                "trustedServiceAccessEnabled": True,
                "ipRules": [{"ipMask": "10.0.0.0/24", "action": "Allow"}],
            }
        },
    )
    assert updated.default_action == "Deny" and updated.trusted_service_access_enabled is True
    assert [r.ip_mask for r in updated.ip_rules] == ["10.0.0.0/24"]
    assert [r.name for r in servicebus.namespaces.list_network_rule_sets(group, namespace.name)] == ["default"]

    with pytest.raises(HttpResponseError) as refused:
        servicebus.namespaces.create_or_update_network_rule_set(group, namespace.name, {"properties": {"defaultAction": "Maybe"}})
    assert refused.value.status_code == 400


def test_private_link_resources(servicebus, group):
    namespace = create(servicebus, group, PREMIUM)
    [resource] = servicebus.private_link_resources.get(group, namespace.name).value
    assert resource.group_id == "namespace"
    assert resource.required_zone_names == ["privatelink.servicebus.windows.net"]
    assert list(servicebus.private_endpoint_connections.list(group, namespace.name)) == []


def test_alias_lifecycle_and_refusals(servicebus, group):
    primary = create(servicebus, group, PREMIUM)
    secondary = create(servicebus, group, PREMIUM)
    standard = create(servicebus, group, STANDARD)
    configs = servicebus.disaster_recovery_configs

    # Pairing needs two different Premium namespaces.
    for partner in (standard.id, primary.id, primary.id.replace(primary.name, unique("missing-"))):
        with pytest.raises(HttpResponseError) as refused:
            configs.create_or_update(group, primary.name, unique("alias-"), {"properties": {"partnerNamespace": partner}})
        assert refused.value.status_code == 400, partner
    with pytest.raises(HttpResponseError) as refused:
        configs.create_or_update(group, primary.name, unique("alias-"), {"properties": {}})
    assert refused.value.status_code == 400

    alias = unique("alias-")
    assert configs.check_name_availability(group, primary.name, {"name": alias}).name_available
    assert configs.check_name_availability(group, primary.name, {"name": "no"}).reason == "InvalidName"
    configs.create_or_update(group, primary.name, alias, {"properties": {"partnerNamespace": secondary.id}})
    taken = configs.check_name_availability(group, primary.name, {"name": alias})
    assert (taken.name_available, taken.reason) == (False, "NameInUse")
    # The alias shares the namespace name space.
    assert not servicebus.namespaces.check_name_availability({"name": alias}).name_available
    with pytest.raises(HttpResponseError) as refused:
        servicebus.namespaces.begin_create_or_update(group, alias, STANDARD).result()
    assert refused.value.status_code == 409

    [listed] = configs.list(group, primary.name)
    assert (listed.name, listed.role, listed.partner_namespace) == (alias, "Primary", secondary.id)
    [mirrored] = configs.list(group, secondary.name)
    assert (mirrored.role, mirrored.partner_namespace.lower()) == ("Secondary", primary.id.lower())

    # The namespace's rules are presented under the alias.
    assert [r.name for r in configs.list_authorization_rules(group, primary.name, alias)] == ["RootManageSharedAccessKey"]
    rule = configs.get_authorization_rule(group, primary.name, alias, "RootManageSharedAccessKey")
    assert rule.rights and sorted(rule.rights) == ["Listen", "Manage", "Send"]
    keys = configs.list_keys(group, primary.name, alias, "RootManageSharedAccessKey")
    assert keys.primary_key == servicebus.namespaces.list_keys(group, primary.name, "RootManageSharedAccessKey").primary_key
    assert f"sb://{primary.name}." in keys.primary_connection_string
    with pytest.raises(ResourceNotFoundError):
        configs.get_authorization_rule(group, primary.name, alias, "nobody")

    # Failover is the secondary's move; breaking the pairing is the primary's.
    with pytest.raises(HttpResponseError) as refused:
        configs.fail_over(group, primary.name, alias)
    assert refused.value.status_code == 400
    with pytest.raises(HttpResponseError) as refused:
        configs.break_pairing(group, secondary.name, alias)
    assert refused.value.status_code == 400

    configs.break_pairing(group, primary.name, alias)
    broken = configs.get(group, primary.name, alias)
    assert broken.role == "PrimaryNotReplicating" and not broken.partner_namespace
    assert list(configs.list(group, secondary.name)) == []

    configs.delete(group, primary.name, alias)
    assert list(configs.list(group, primary.name)) == []
    assert servicebus.namespaces.check_name_availability({"name": alias}).name_available


def test_deleting_the_primary_alias_dissolves_the_pairing(servicebus, group):
    primary = create(servicebus, group, PREMIUM)
    secondary = create(servicebus, group, PREMIUM)
    alias = unique("alias-")
    configs = servicebus.disaster_recovery_configs
    configs.create_or_update(group, primary.name, alias, {"properties": {"partnerNamespace": secondary.id, "alternateName": "spare"}})
    assert configs.get(group, primary.name, alias).alternate_name == "spare"
    configs.delete(group, primary.name, alias)
    assert list(configs.list(group, secondary.name)) == []
    assert servicebus.namespaces.check_name_availability({"name": alias}).name_available


def test_migration_copies_entities_and_refusals(servicebus, group):
    standard = create(servicebus, group, STANDARD)
    premium = create(servicebus, group, PREMIUM)
    other_standard = create(servicebus, group, STANDARD)
    migrations = servicebus.migration_configs

    servicebus.queues.create_or_update(group, standard.name, "jobs", {"properties": {"maxDeliveryCount": 7}})
    servicebus.topics.create_or_update(group, standard.name, "events", {})
    servicebus.subscriptions.create_or_update(group, standard.name, "events", "audit", {"properties": {"maxDeliveryCount": 2}})
    servicebus.rules.create_or_update(group, standard.name, "events", "audit", "errors", {"properties": {"sqlFilter": {"sqlExpression": "level = 'error'"}}})

    def start(source, target, post=None):
        return migrations.begin_create_and_start_migration(
            group, source, "$default", {"properties": {"targetNamespace": target, "postMigrationName": post or unique("old-")}}
        ).result()

    # From Standard, to an empty Premium, under a free post-migration name.
    for source, target in [(premium.name, premium.id), (standard.name, other_standard.id)]:
        with pytest.raises(HttpResponseError) as refused:
            start(source, target)
        assert refused.value.status_code == 400
    with pytest.raises(HttpResponseError) as refused:
        start(standard.name, premium.id, post=other_standard.name)
    assert refused.value.status_code == 409
    with pytest.raises(HttpResponseError) as refused:
        start(standard.name, premium.id, post="bad_name")
    assert refused.value.status_code == 400

    config = start(standard.name, premium.id)
    assert (config.migration_state, config.target_namespace) == ("Active", premium.id)
    assert servicebus.queues.get(group, premium.name, "jobs").max_delivery_count == 7
    assert servicebus.subscriptions.get(group, premium.name, "events", "audit").max_delivery_count == 2
    rule = servicebus.rules.get(group, premium.name, "events", "audit", "errors")
    assert rule.sql_filter.sql_expression == "level = 'error'"
    [listed] = migrations.list(group, standard.name)
    assert listed.name == "$default"

    # One migration at a time.
    with pytest.raises(HttpResponseError) as refused:
        start(standard.name, premium.id)
    assert refused.value.status_code == 409

    migrations.revert(group, standard.name, "$default")
    assert list(migrations.list(group, standard.name)) == []
    # Reverting leaves both namespaces as they were.
    assert servicebus.queues.get(group, standard.name, "jobs").max_delivery_count == 7


def test_post_forms_of_authorization_rule_reads(emulator, servicebus, group, credential):
    import requests

    namespace = create(servicebus, group, STANDARD)
    servicebus.queues.create_or_update(group, namespace.name, "jobs", {})
    servicebus.queues.create_or_update_authorization_rule(group, namespace.name, "jobs", "reader", {"properties": {"rights": ["Listen"]}})
    token = credential.get_token(emulator.base + "/.default").token
    base = (
        f"{emulator.base}/subscriptions/{emulator.subscription}/resourceGroups/{group}"
        f"/providers/Microsoft.ServiceBus/namespaces/{namespace.name}"
    )

    def post(path):
        return requests.post(
            f"{base}{path}?api-version=2021-11-01", headers={"Authorization": f"Bearer {token}"}, verify=emulator.ca, json={}
        )

    listed = post("/AuthorizationRules")
    assert listed.status_code == 200 and [r["name"] for r in listed.json()["value"]] == ["RootManageSharedAccessKey"]
    one = post("/queues/jobs/authorizationRules/reader")
    assert one.status_code == 200 and one.json()["properties"]["rights"] == ["Listen"]
    assert post("/queues/jobs/authorizationRules/nobody").status_code == 404
