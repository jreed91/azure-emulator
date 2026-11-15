"""Event Hubs geo-disaster-recovery aliases: pairing, the alias's name,
failover and break-pairing, and the namespace's authorization rules and keys
presented under the alias.

Expected values follow the Event Hubs management REST reference
(Disaster Recovery Configs) and the geo-disaster recovery documentation:
a pairing joins two namespaces of the same tier, Standard or Premium."""

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError

from conftest import unique

BASIC = {"location": "eastus", "sku": {"name": "Basic"}}
STANDARD = {"location": "eastus", "sku": {"name": "Standard"}}
PREMIUM = {"location": "eastus", "sku": {"name": "Premium"}}


def create(eventhub, group, body):
    return eventhub.namespaces.begin_create_or_update(group, unique("eh-"), body).result()


def test_alias_lifecycle_and_refusals(eventhub, group):
    primary = create(eventhub, group, STANDARD)
    secondary = create(eventhub, group, STANDARD)
    basic = create(eventhub, group, BASIC)
    premium = create(eventhub, group, PREMIUM)
    configs = eventhub.disaster_recovery_configs

    # A pairing needs two different namespaces of one tier that supports it.
    for partner in (basic.id, premium.id, primary.id, primary.id.replace(primary.name, unique("missing-"))):
        with pytest.raises(HttpResponseError) as refused:
            configs.create_or_update(group, primary.name, unique("alias-"), {"properties": {"partnerNamespace": partner}})
        assert refused.value.status_code == 400, partner
    with pytest.raises(HttpResponseError) as refused:
        configs.create_or_update(group, basic.name, unique("alias-"), {"properties": {"partnerNamespace": create(eventhub, group, BASIC).id}})
    assert refused.value.status_code == 400
    with pytest.raises(HttpResponseError) as refused:
        configs.create_or_update(group, primary.name, unique("alias-"), {"properties": {}})
    assert refused.value.status_code == 400
    with pytest.raises(ResourceNotFoundError):
        configs.check_name_availability(group, unique("missing-"), {"name": unique("alias-")})
    assert list(configs.list(group, primary.name)) == []

    alias = unique("alias-")
    assert configs.check_name_availability(group, primary.name, {"name": alias}).name_available
    assert configs.check_name_availability(group, primary.name, {"name": "no"}).reason == "InvalidName"
    created = configs.create_or_update(group, primary.name, alias, {"properties": {"partnerNamespace": secondary.id}})
    assert (created.name, created.role, created.partner_namespace, created.provisioning_state) == (
        alias, "Primary", secondary.id, "Succeeded")
    assert created.type == "Microsoft.EventHub/namespaces/disasterRecoveryConfigs"
    taken = configs.check_name_availability(group, primary.name, {"name": alias})
    assert (taken.name_available, taken.reason) == (False, "NameInUse")
    # The alias shares the namespace name space, with Service Bus too.
    assert not eventhub.namespaces.check_name_availability({"name": alias}).name_available
    with pytest.raises(HttpResponseError) as refused:
        eventhub.namespaces.begin_create_or_update(group, alias, STANDARD).result()
    assert refused.value.status_code == 409

    got = configs.get(group, primary.name, alias)
    assert (got.role, got.partner_namespace, got.pending_replication_operations_count) == ("Primary", secondary.id, 0)
    [listed] = configs.list(group, primary.name)
    assert (listed.name, listed.role) == (alias, "Primary")
    [mirrored] = configs.list(group, secondary.name)
    assert (mirrored.name, mirrored.role, mirrored.partner_namespace.lower()) == (alias, "Secondary", primary.id.lower())

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
    with pytest.raises(ResourceNotFoundError):
        configs.get(group, secondary.name, alias)

    configs.delete(group, primary.name, alias)
    assert list(configs.list(group, primary.name)) == []
    with pytest.raises(ResourceNotFoundError):
        configs.get(group, primary.name, alias)
    assert eventhub.namespaces.check_name_availability({"name": alias}).name_available


def test_failover_leaves_the_secondary_as_the_only_holder(eventhub, group):
    primary = create(eventhub, group, PREMIUM)
    secondary = create(eventhub, group, PREMIUM)
    alias = unique("alias-")
    configs = eventhub.disaster_recovery_configs
    configs.create_or_update(group, primary.name, alias, {"properties": {"partnerNamespace": secondary.id, "alternateName": "spare"}})
    assert configs.get(group, primary.name, alias).alternate_name == "spare"

    configs.fail_over(group, secondary.name, alias)
    promoted = configs.get(group, secondary.name, alias)
    assert promoted.role == "PrimaryNotReplicating" and not promoted.partner_namespace
    assert list(configs.list(group, primary.name)) == []
    # The alias is still taken until its last configuration goes.
    assert not eventhub.namespaces.check_name_availability({"name": alias}).name_available
    configs.delete(group, secondary.name, alias)
    assert eventhub.namespaces.check_name_availability({"name": alias}).name_available


def test_deleting_the_primary_alias_dissolves_the_pairing(eventhub, group):
    primary = create(eventhub, group, STANDARD)
    secondary = create(eventhub, group, STANDARD)
    alias = unique("alias-")
    configs = eventhub.disaster_recovery_configs
    configs.create_or_update(group, primary.name, alias, {"properties": {"partnerNamespace": secondary.id}})
    configs.delete(group, primary.name, alias)
    assert list(configs.list(group, secondary.name)) == []
    assert eventhub.namespaces.check_name_availability({"name": alias}).name_available


def test_alias_authorization_rules_and_keys(eventhub, group):
    primary = create(eventhub, group, STANDARD)
    secondary = create(eventhub, group, STANDARD)
    alias = unique("alias-")
    configs = eventhub.disaster_recovery_configs
    eventhub.namespaces.create_or_update_authorization_rule(group, primary.name, "sender", {"properties": {"rights": ["Send"]}})
    configs.create_or_update(group, primary.name, alias, {"properties": {"partnerNamespace": secondary.id}})

    # The namespace's rules are presented under the alias.
    rules = {r.name: r for r in configs.list_authorization_rules(group, primary.name, alias)}
    assert sorted(rules) == ["RootManageSharedAccessKey", "sender"]
    assert rules["sender"].type == "Microsoft.EventHub/namespaces/disasterRecoveryConfigs/authorizationRules"
    assert rules["sender"].id.lower() == f"{primary.id}/disasterRecoveryConfigs/{alias}/authorizationRules/sender".lower()
    rule = configs.get_authorization_rule(group, primary.name, alias, "RootManageSharedAccessKey")
    assert sorted(rule.rights) == ["Listen", "Manage", "Send"]
    assert configs.get_authorization_rule(group, primary.name, alias, "sender").rights == ["Send"]

    keys = configs.list_keys(group, primary.name, alias, "sender")
    namespace_keys = eventhub.namespaces.list_keys(group, primary.name, "sender")
    assert (keys.primary_key, keys.secondary_key) == (namespace_keys.primary_key, namespace_keys.secondary_key)
    assert keys.key_name == "sender"
    assert f"sb://{primary.name}." in keys.primary_connection_string
    assert f"sb://{alias}." in keys.alias_primary_connection_string
    assert f"SharedAccessKey={keys.secondary_key}" in keys.alias_secondary_connection_string

    with pytest.raises(ResourceNotFoundError):
        configs.get_authorization_rule(group, primary.name, alias, "nobody")
    with pytest.raises(ResourceNotFoundError):
        configs.list_keys(group, primary.name, alias, "nobody")
    with pytest.raises(ResourceNotFoundError):
        list(configs.list_authorization_rules(group, primary.name, unique("missing-")))
