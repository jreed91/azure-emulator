"""Event Hubs on Resource Manager: namespaces, event hubs, consumer groups,
authorization rules, network rule sets and private link resources."""

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError

from conftest import unique

STANDARD = {"location": "eastus", "sku": {"name": "Standard"}}


@pytest.fixture
def namespace(eventhub, group):
    return eventhub.namespaces.begin_create_or_update(group, unique("eh-"), STANDARD).result()


def test_namespace_get_update_list_and_delete(eventhub, group, namespace):
    got = eventhub.namespaces.get(group, namespace.name)
    assert (got.name, got.sku.name, got.location) == (namespace.name, "Standard", "eastus")
    assert got.service_bus_endpoint.startswith(f"https://{namespace.name}.")

    updated = eventhub.namespaces.update(group, namespace.name, {"tags": {"team": "data"}})
    assert updated.tags == {"team": "data"}
    assert eventhub.namespaces.get(group, namespace.name).tags == {"team": "data"}

    assert namespace.name in [n.name for n in eventhub.namespaces.list_by_resource_group(group)]
    assert namespace.name in [n.name for n in eventhub.namespaces.list()]

    eventhub.namespaces.begin_delete(group, namespace.name).result()
    with pytest.raises(ResourceNotFoundError):
        eventhub.namespaces.get(group, namespace.name)
    assert namespace.name not in [n.name for n in eventhub.namespaces.list_by_resource_group(group)]


def test_namespace_name_availability(eventhub, namespace):
    assert eventhub.namespaces.check_name_availability({"name": unique("eh-")}).name_available
    taken = eventhub.namespaces.check_name_availability({"name": namespace.name})
    assert (taken.name_available, taken.reason) == (False, "NameInUse")
    assert eventhub.namespaces.check_name_availability({"name": "x"}).reason == "InvalidName"


def test_namespace_authorization_rules(eventhub, group, namespace):
    rules = eventhub.namespaces
    rule = rules.create_or_update_authorization_rule(group, namespace.name, "sender", {"properties": {"rights": ["Send"]}})
    assert (rule.name, rule.rights) == ("sender", ["Send"])
    assert rules.get_authorization_rule(group, namespace.name, "sender").rights == ["Send"]
    assert sorted(r.name for r in rules.list_authorization_rules(group, namespace.name)) == ["RootManageSharedAccessKey", "sender"]
    keys = rules.list_keys(group, namespace.name, "sender")
    assert "SharedAccessKeyName=sender" in keys.primary_connection_string

    rules.delete_authorization_rule(group, namespace.name, "sender")
    with pytest.raises(ResourceNotFoundError):
        rules.get_authorization_rule(group, namespace.name, "sender")
    # Rights must be Listen, Send or Manage, and Manage needs the other two.
    with pytest.raises(HttpResponseError) as refused:
        rules.create_or_update_authorization_rule(group, namespace.name, "bad", {"properties": {"rights": ["Manage"]}})
    assert refused.value.status_code == 400


def test_event_hub_lifecycle_and_rules(eventhub, group, namespace):
    hubs = eventhub.event_hubs
    hubs.create_or_update(group, namespace.name, "orders", {"properties": {"partitionCount": 2, "messageRetentionInDays": 3}})
    got = hubs.get(group, namespace.name, "orders")
    assert (got.name, got.partition_count, got.partition_ids) == ("orders", 2, ["0", "1"])
    assert [h.name for h in hubs.list_by_namespace(group, namespace.name)] == ["orders"]

    rule = hubs.create_or_update_authorization_rule(group, namespace.name, "orders", "reader", {"properties": {"rights": ["Listen"]}})
    assert rule.rights == ["Listen"]
    assert hubs.get_authorization_rule(group, namespace.name, "orders", "reader").rights == ["Listen"]
    assert [r.name for r in hubs.list_authorization_rules(group, namespace.name, "orders")] == ["reader"]
    keys = hubs.list_keys(group, namespace.name, "orders", "reader")
    assert "EntityPath=orders" in keys.primary_connection_string
    renewed = hubs.regenerate_keys(group, namespace.name, "orders", "reader", {"keyType": "PrimaryKey"})
    assert renewed.primary_key != keys.primary_key and renewed.secondary_key == keys.secondary_key
    hubs.delete_authorization_rule(group, namespace.name, "orders", "reader")
    with pytest.raises(ResourceNotFoundError):
        hubs.get_authorization_rule(group, namespace.name, "orders", "reader")

    hubs.delete(group, namespace.name, "orders")
    with pytest.raises(ResourceNotFoundError):
        hubs.get(group, namespace.name, "orders")
    assert list(hubs.list_by_namespace(group, namespace.name)) == []


def test_consumer_group_get_and_delete(eventhub, group, namespace):
    eventhub.event_hubs.create_or_update(group, namespace.name, "clicks", {"properties": {"partitionCount": 1}})
    groups = eventhub.consumer_groups
    groups.create_or_update(group, namespace.name, "clicks", "analytics", {"properties": {"userMetadata": "dashboards"}})
    got = groups.get(group, namespace.name, "clicks", "analytics")
    assert (got.name, got.user_metadata) == ("analytics", "dashboards")
    groups.delete(group, namespace.name, "clicks", "analytics")
    with pytest.raises(ResourceNotFoundError):
        groups.get(group, namespace.name, "clicks", "analytics")
    assert [g.name for g in groups.list_by_event_hub(group, namespace.name, "clicks")] == ["$Default"]


def test_network_rule_set(eventhub, group, namespace):
    default = eventhub.namespaces.get_network_rule_set(group, namespace.name)
    assert (default.name, default.default_action) == ("default", "Allow")
    updated = eventhub.namespaces.create_or_update_network_rule_set(
        group,
        namespace.name,
        {"properties": {"defaultAction": "Deny", "ipRules": [{"ipMask": "10.0.0.0/24", "action": "Allow"}]}},
    )
    assert (updated.default_action, [r.ip_mask for r in updated.ip_rules]) == ("Deny", ["10.0.0.0/24"])
    assert eventhub.namespaces.get_network_rule_set(group, namespace.name).default_action == "Deny"
    assert [r.name for r in eventhub.namespaces.list_network_rule_set(group, namespace.name).value] == ["default"]
