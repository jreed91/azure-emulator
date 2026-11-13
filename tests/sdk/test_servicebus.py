"""Service Bus: the control plane (there is no messaging data plane yet)."""

import pytest

from conftest import unique


@pytest.fixture
def namespace(servicebus, group):
    name = unique("sb-")
    return servicebus.namespaces.begin_create_or_update(group, name, {"location": "eastus", "sku": {"name": "Standard"}}).result()


def test_namespace(emulator, servicebus, group, namespace):
    assert namespace.provisioning_state == "Succeeded"
    assert namespace.sku.tier == "Standard"
    assert not servicebus.namespaces.check_name_availability({"name": namespace.name}).name_available

    keys = servicebus.namespaces.list_keys(group, namespace.name, "RootManageSharedAccessKey")
    assert keys.primary_connection_string.startswith(
        f"Endpoint=sb://{namespace.name}.servicebus.azure.localhost:{emulator.port}/;"
    )

    servicebus.namespaces.begin_delete(group, namespace.name).result()
    assert servicebus.namespaces.check_name_availability({"name": namespace.name}).name_available


def test_queue(servicebus, group, namespace):
    queue = servicebus.queues.create_or_update(group, namespace.name, "jobs", {"properties": {"maxDeliveryCount": 4}})
    assert queue.max_delivery_count == 4
    assert queue.lock_duration.total_seconds() == 60
    assert queue.message_count == 0
    assert [q.name for q in servicebus.queues.list_by_namespace(group, namespace.name)] == ["jobs"]
    servicebus.queues.delete(group, namespace.name, "jobs")
    assert list(servicebus.queues.list_by_namespace(group, namespace.name)) == []


def test_topic_subscription_and_rule(servicebus, group, namespace):
    servicebus.topics.create_or_update(group, namespace.name, "events", {})
    servicebus.subscriptions.create_or_update(group, namespace.name, "events", "audit", {})
    assert servicebus.topics.get(group, namespace.name, "events").subscription_count == 1
    assert [r.name for r in servicebus.rules.list_by_subscriptions(group, namespace.name, "events", "audit")] == ["$Default"]

    rule = servicebus.rules.create_or_update(
        group, namespace.name, "events", "audit", "errors",
        {"properties": {"filterType": "SqlFilter", "sqlFilter": {"sqlExpression": "level = 'error'"}}},
    )
    assert rule.sql_filter.compatibility_level == 20


def test_geo_disaster_recovery_alias(servicebus, group):
    premium = {"location": "eastus", "sku": {"name": "Premium"}}
    primary = servicebus.namespaces.begin_create_or_update(group, unique("sb-"), premium).result()
    secondary = servicebus.namespaces.begin_create_or_update(group, unique("sb-"), premium).result()
    alias = unique("alias-")

    config = servicebus.disaster_recovery_configs.create_or_update(
        group, primary.name, alias, {"properties": {"partnerNamespace": secondary.id}}
    )
    assert config.role == "Primary"
    assert servicebus.disaster_recovery_configs.get(group, secondary.name, alias).role == "Secondary"

    keys = servicebus.disaster_recovery_configs.list_keys(group, primary.name, alias, "RootManageSharedAccessKey")
    assert keys.alias_primary_connection_string.startswith(f"Endpoint=sb://{alias}.servicebus.")

    servicebus.disaster_recovery_configs.fail_over(group, secondary.name, alias)
    assert servicebus.disaster_recovery_configs.get(group, secondary.name, alias).role == "PrimaryNotReplicating"
