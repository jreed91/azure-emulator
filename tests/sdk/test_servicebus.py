"""Service Bus: the control plane, and messaging over AMQP with the azure-servicebus SDK."""

import datetime

import pytest
from azure.mgmt.servicebus.models import CorrelationFilter, Rule
from azure.servicebus import ServiceBusClient, ServiceBusMessage, ServiceBusReceiveMode, ServiceBusSubQueue

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
        f"Endpoint=sb://{namespace.name}.servicebus.azure.localhost:{emulator.amqp_port}/;"
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


def test_standard_to_premium_migration(servicebus, group):
    standard = servicebus.namespaces.begin_create_or_update(group, unique("sb-"), {"location": "eastus", "sku": {"name": "Standard"}}).result()
    premium = servicebus.namespaces.begin_create_or_update(group, unique("sb-"), {"location": "eastus", "sku": {"name": "Premium"}}).result()
    servicebus.queues.create_or_update(group, standard.name, "jobs", {"properties": {"maxDeliveryCount": 3}})

    config = servicebus.migration_configs.begin_create_and_start_migration(
        group, standard.name, "$default",
        {"properties": {"targetNamespace": premium.id, "postMigrationName": unique("old-")}},
    ).result()
    assert config.migration_state == "Active"
    assert servicebus.queues.get(group, premium.name, "jobs").max_delivery_count == 3

    servicebus.migration_configs.revert(group, standard.name, "$default")
    assert list(servicebus.migration_configs.list(group, standard.name)) == []


def test_authorization_rules_through_post(servicebus, group, namespace):
    rules = list(servicebus.namespaces.list_authorization_rules(group, namespace.name))
    assert [r.name for r in rules] == ["RootManageSharedAccessKey"]


def _client(servicebus, group, namespace):
    keys = servicebus.namespaces.list_keys(group, namespace.name, "RootManageSharedAccessKey")
    return ServiceBusClient.from_connection_string(keys.primary_connection_string)


def test_send_receive_complete(servicebus, group, namespace):
    servicebus.queues.create_or_update(group, namespace.name, "jobs", {})
    with _client(servicebus, group, namespace) as client:
        with client.get_queue_sender("jobs") as sender:
            sender.send_messages(ServiceBusMessage("one", application_properties={"kind": "greeting"}))
            sender.send_messages([ServiceBusMessage("two"), ServiceBusMessage("three")])
        assert servicebus.queues.get(group, namespace.name, "jobs").message_count == 3
        with client.get_queue_receiver("jobs", max_wait_time=2) as receiver:
            got = receiver.receive_messages(max_message_count=5, max_wait_time=2)
            assert [str(m) for m in got] == ["one", "two", "three"]
            assert [m.sequence_number for m in got] == [1, 2, 3]
            assert [m.delivery_count for m in got] == [0, 0, 0]
            assert got[0].application_properties[b"kind"] == b"greeting"
            for message in got:
                receiver.complete_message(message)
            assert receiver.receive_messages(max_message_count=5, max_wait_time=1) == []
    assert servicebus.queues.get(group, namespace.name, "jobs").message_count == 0


def test_abandon_renew_and_dead_letter(servicebus, group, namespace):
    servicebus.queues.create_or_update(group, namespace.name, "jobs", {})
    with _client(servicebus, group, namespace) as client:
        with client.get_queue_sender("jobs") as sender:
            sender.send_messages(ServiceBusMessage("work"))
        with client.get_queue_receiver("jobs", max_wait_time=2) as receiver:
            message = receiver.receive_messages(max_message_count=1, max_wait_time=2)[0]
            receiver.abandon_message(message)
            message = receiver.receive_messages(max_message_count=1, max_wait_time=2)[0]
            assert str(message) == "work" and message.delivery_count == 1
            before = message.locked_until_utc
            receiver.renew_message_lock(message)
            assert message.locked_until_utc >= before
            receiver.dead_letter_message(message, reason="poison", error_description="cannot parse")
        details = servicebus.queues.get(group, namespace.name, "jobs").count_details
        assert (details.active_message_count, details.dead_letter_message_count) == (0, 1)
        with client.get_queue_receiver("jobs", sub_queue=ServiceBusSubQueue.DEAD_LETTER, max_wait_time=2) as receiver:
            [dead] = receiver.receive_messages(max_message_count=5, max_wait_time=2)
            assert str(dead) == "work"
            assert (dead.dead_letter_reason, dead.dead_letter_error_description) == ("poison", "cannot parse")
            receiver.complete_message(dead)


def test_max_delivery_count_dead_letters(servicebus, group, namespace):
    servicebus.queues.create_or_update(group, namespace.name, "jobs", {"properties": {"maxDeliveryCount": 1}})
    with _client(servicebus, group, namespace) as client:
        with client.get_queue_sender("jobs") as sender:
            sender.send_messages(ServiceBusMessage("flaky"))
        with client.get_queue_receiver("jobs", max_wait_time=2) as receiver:
            receiver.abandon_message(receiver.receive_messages(max_message_count=1, max_wait_time=2)[0])
            assert receiver.receive_messages(max_message_count=1, max_wait_time=1) == []
        with client.get_queue_receiver("jobs", sub_queue=ServiceBusSubQueue.DEAD_LETTER, max_wait_time=2) as receiver:
            [dead] = receiver.receive_messages(max_message_count=1, max_wait_time=2)
            assert dead.dead_letter_reason == "MaxDeliveryCountExceeded"


def test_receive_and_delete_and_peek(servicebus, group, namespace):
    servicebus.queues.create_or_update(group, namespace.name, "jobs", {})
    with _client(servicebus, group, namespace) as client:
        with client.get_queue_sender("jobs") as sender:
            sender.send_messages([ServiceBusMessage("a"), ServiceBusMessage("b")])
        with client.get_queue_receiver("jobs", receive_mode=ServiceBusReceiveMode.RECEIVE_AND_DELETE, max_wait_time=2) as receiver:
            assert [str(m) for m in receiver.peek_messages(max_message_count=5)] == ["a", "b"]
            assert [str(m) for m in receiver.receive_messages(max_message_count=5, max_wait_time=2)] == ["a", "b"]
            assert receiver.peek_messages(max_message_count=5) == []


def test_schedule_defer_and_duplicate_detection(servicebus, group, namespace):
    servicebus.queues.create_or_update(group, namespace.name, "jobs", {"properties": {"requiresDuplicateDetection": True}})
    now = datetime.datetime.now(datetime.timezone.utc)
    with _client(servicebus, group, namespace) as client:
        with client.get_queue_sender("jobs") as sender:
            sender.send_messages(ServiceBusMessage("once", message_id="m-1"))
            sender.send_messages(ServiceBusMessage("once", message_id="m-1"))
            [later] = sender.schedule_messages(ServiceBusMessage("later"), now + datetime.timedelta(hours=1))
            assert servicebus.queues.get(group, namespace.name, "jobs").count_details.scheduled_message_count == 1
            sender.cancel_scheduled_messages(later)
        with client.get_queue_receiver("jobs", max_wait_time=2) as receiver:
            [message] = receiver.receive_messages(max_message_count=5, max_wait_time=2)
            receiver.defer_message(message)
            assert receiver.receive_messages(max_message_count=5, max_wait_time=1) == []
            [deferred] = receiver.receive_deferred_messages([message.sequence_number])
            assert str(deferred) == "once"
            receiver.complete_message(deferred)


def test_topic_fan_out_with_correlation_filter(servicebus, group, namespace):
    servicebus.topics.create_or_update(group, namespace.name, "events", {})
    servicebus.subscriptions.create_or_update(group, namespace.name, "events", "everything", {})
    servicebus.subscriptions.create_or_update(group, namespace.name, "events", "errors", {})
    servicebus.rules.delete(group, namespace.name, "events", "errors", "$Default")
    servicebus.rules.create_or_update(
        group, namespace.name, "events", "errors", "level",
        Rule(filter_type="CorrelationFilter", correlation_filter=CorrelationFilter(properties={"level": "error"})),
    )
    with _client(servicebus, group, namespace) as client:
        with client.get_topic_sender("events") as sender:
            sender.send_messages([
                ServiceBusMessage("boom", application_properties={"level": "error"}),
                ServiceBusMessage("fine", application_properties={"level": "info"}),
            ])
        for subscription, expected in [("everything", ["boom", "fine"]), ("errors", ["boom"])]:
            with client.get_subscription_receiver("events", subscription, max_wait_time=2) as receiver:
                got = receiver.receive_messages(max_message_count=5, max_wait_time=2)
                assert sorted(str(m) for m in got) == expected
                for message in got:
                    receiver.complete_message(message)


def test_wrong_key_is_refused(servicebus, group, namespace):
    servicebus.queues.create_or_update(group, namespace.name, "jobs", {})
    keys = servicebus.namespaces.list_keys(group, namespace.name, "RootManageSharedAccessKey")
    forged = keys.primary_connection_string.replace(keys.primary_key, "A" * 43 + "=")
    with ServiceBusClient.from_connection_string(forged, retry_total=0) as client:
        with pytest.raises(Exception):
            with client.get_queue_sender("jobs") as sender:
                sender.send_messages(ServiceBusMessage("nope"))


def test_sql_filters_and_actions(servicebus, group, namespace):
    from azure.core.exceptions import HttpResponseError
    from azure.mgmt.servicebus.models import Action, SqlFilter

    servicebus.topics.create_or_update(group, namespace.name, "orders", {})
    servicebus.subscriptions.create_or_update(group, namespace.name, "orders", "big", {})
    servicebus.rules.delete(group, namespace.name, "orders", "big", "$Default")
    servicebus.rules.create_or_update(
        group, namespace.name, "orders", "big", "large-red",
        Rule(
            filter_type="SqlFilter",
            sql_filter=SqlFilter(sql_expression="quantity > 10 AND color IN ('red', 'crimson') AND sys.Label LIKE 'ord%'"),
            action=Action(sql_expression="SET priority = 'high'; SET quantity = quantity * 2; REMOVE color"),
        ),
    )
    with pytest.raises(HttpResponseError):
        servicebus.rules.create_or_update(
            group, namespace.name, "orders", "big", "broken",
            Rule(filter_type="SqlFilter", sql_filter=SqlFilter(sql_expression="quantity >")),
        )
    with _client(servicebus, group, namespace) as client:
        with client.get_topic_sender("orders") as sender:
            sender.send_messages([
                ServiceBusMessage("match", subject="order", application_properties={"quantity": 12, "color": "red"}),
                ServiceBusMessage("small", subject="order", application_properties={"quantity": 2, "color": "red"}),
                ServiceBusMessage("no-color", subject="order", application_properties={"quantity": 50}),
            ])
        with client.get_subscription_receiver("orders", "big", max_wait_time=2) as receiver:
            got = receiver.receive_messages(max_message_count=5, max_wait_time=2)
            assert [str(m) for m in got] == ["match"]
            properties = got[0].application_properties
            assert properties[b"priority"] == b"high"
            assert properties[b"quantity"] == 24
            assert b"color" not in properties
            receiver.complete_message(got[0])


def test_sessions(servicebus, group, namespace):
    from azure.servicebus import NEXT_AVAILABLE_SESSION
    from azure.servicebus.exceptions import ServiceBusError

    servicebus.queues.create_or_update(group, namespace.name, "orders", {"properties": {"requiresSession": True}})
    with _client(servicebus, group, namespace) as client:
        with client.get_queue_sender("orders") as sender:
            sender.send_messages([
                ServiceBusMessage("a1", session_id="a"),
                ServiceBusMessage("b1", session_id="b"),
                ServiceBusMessage("a2", session_id="a"),
            ])
            with pytest.raises(ServiceBusError):
                sender.send_messages(ServiceBusMessage("no session"))

        with client.get_queue_receiver("orders", session_id="b", max_wait_time=2) as receiver:
            got = receiver.receive_messages(max_message_count=5, max_wait_time=2)
            assert [str(m) for m in got] == ["b1"]
            assert receiver.session.session_id == "b"
            receiver.session.set_state(b"step-1")
            assert receiver.session.get_state() == b"step-1"
            before = receiver.session.locked_until_utc
            receiver.session.renew_lock()
            assert receiver.session.locked_until_utc >= before
            for message in got:
                receiver.complete_message(message)

        with client.get_queue_receiver("orders", session_id=NEXT_AVAILABLE_SESSION, max_wait_time=2) as receiver:
            assert receiver.session.session_id == "a"
            # While this receiver holds session a, nobody else can lock it.
            with client.get_queue_receiver("orders", session_id="a", max_wait_time=2) as other:
                with pytest.raises(ServiceBusError):
                    other.receive_messages(max_message_count=1, max_wait_time=1)
            got = receiver.receive_messages(max_message_count=5, max_wait_time=2)
            assert [str(m) for m in got] == ["a1", "a2"]
            for message in got:
                receiver.complete_message(message)

        # The lock was released when the receiver closed, and the state kept.
        with client.get_queue_receiver("orders", session_id="b", max_wait_time=2) as receiver:
            assert receiver.session.get_state() == b"step-1"

        with client.get_queue_receiver("orders", max_wait_time=1) as receiver:
            with pytest.raises(ServiceBusError):
                receiver.receive_messages(max_message_count=1, max_wait_time=1)


def test_tls_with_entra_id_and_with_a_shared_access_signature(emulator):
    """Clients that connect by namespace name use TLS on port 5671, which they
    do not let a test change: this test runs its own emulator listening there
    (sharing the suite's CA), and is the only one that does."""
    from azure.identity import ClientSecretCredential
    from azure.mgmt.resource.resources import ResourceManagementClient
    from azure.mgmt.servicebus import ServiceBusManagementClient

    from conftest import TENANT, run_emulator

    with run_emulator(emulator.home, amqps_port=5671) as tls:
        credential = ClientSecretCredential(TENANT, "sdk-tests", "secret", authority=tls.base, disable_instance_discovery=True)
        ResourceManagementClient(credential, tls.subscription, **tls.arm).resource_groups.create_or_update("tls", {"location": "eastus"})
        management = ServiceBusManagementClient(credential, tls.subscription, **tls.arm)
        name = unique("tls-")
        management.namespaces.begin_create_or_update("tls", name, {"location": "eastus"}).result()
        management.queues.create_or_update("tls", name, "jobs", {})
        host = f"{name}.servicebus.azure.localhost"

        with ServiceBusClient(host, credential, connection_verify=tls.ca) as client:
            with client.get_queue_sender("jobs") as sender:
                sender.send_messages(ServiceBusMessage("entra"))

        # Without UseDevelopmentEmulator the SDKs connect over TLS.
        keys = management.namespaces.list_keys("tls", name, "RootManageSharedAccessKey")
        connection = keys.primary_connection_string.replace(";UseDevelopmentEmulator=true", "")
        with ServiceBusClient.from_connection_string(connection, connection_verify=tls.ca) as client:
            with client.get_queue_receiver("jobs", max_wait_time=2) as receiver:
                got = receiver.receive_messages(max_message_count=5, max_wait_time=2)
                assert [str(m) for m in got] == ["entra"]
                receiver.complete_message(got[0])


def test_amqp_over_websockets(emulator, credential, servicebus, group, namespace):
    from azure.servicebus import TransportType

    servicebus.queues.create_or_update(group, namespace.name, "jobs", {})
    host = f"{namespace.name}.servicebus.azure.localhost"
    # Over WebSockets the SDKs reach the emulator's HTTPS port, at
    # /$servicebus/websocket.
    with ServiceBusClient(
        host,
        credential,
        transport_type=TransportType.AmqpOverWebsocket,
        custom_endpoint_address=f"https://{host}:{emulator.port}",
        connection_verify=emulator.ca,
    ) as client:
        with client.get_queue_sender("jobs") as sender:
            sender.send_messages(ServiceBusMessage("over websockets"))
        with client.get_queue_receiver("jobs", max_wait_time=2) as receiver:
            got = receiver.receive_messages(max_message_count=5, max_wait_time=2)
            assert [str(m) for m in got] == ["over websockets"]
            receiver.complete_message(got[0])


def test_tier_restrictions(servicebus, group):
    from azure.core.exceptions import HttpResponseError

    basic = servicebus.namespaces.begin_create_or_update(group, unique("sb-"), {"location": "eastus", "sku": {"name": "Basic"}}).result()
    servicebus.queues.create_or_update(group, basic.name, "jobs", {})
    for refused in [
        lambda: servicebus.topics.create_or_update(group, basic.name, "events", {}),
        lambda: servicebus.queues.create_or_update(group, basic.name, "sessions", {"properties": {"requiresSession": True}}),
        lambda: servicebus.queues.create_or_update(group, basic.name, "dedup", {"properties": {"requiresDuplicateDetection": True}}),
        lambda: servicebus.queues.create_or_update(group, basic.name, "forward", {"properties": {"forwardTo": "jobs"}}),
        lambda: servicebus.queues.create_or_update(group, basic.name, "big", {"properties": {"maxMessageSizeInKilobytes": 2048}}),
    ]:
        with pytest.raises(HttpResponseError):
            refused()

    premium = servicebus.namespaces.begin_create_or_update(group, unique("sb-"), {"location": "eastus", "sku": {"name": "Premium"}}).result()
    assert servicebus.queues.create_or_update(group, premium.name, "jobs", {}).max_message_size_in_kilobytes == 1024
    big = servicebus.queues.create_or_update(group, premium.name, "big", {"properties": {"maxMessageSizeInKilobytes": 4096}})
    assert big.max_message_size_in_kilobytes == 4096
    with pytest.raises(HttpResponseError):
        servicebus.queues.create_or_update(group, premium.name, "huge", {"properties": {"maxMessageSizeInKilobytes": 200000}})

    keys = servicebus.namespaces.list_keys(group, premium.name, "RootManageSharedAccessKey")
    with ServiceBusClient.from_connection_string(keys.primary_connection_string) as client:
        with client.get_queue_sender("big") as sender:
            # Well above the Standard limit, within this queue's 4 MB.
            sender.send_messages(ServiceBusMessage(b"x" * 3 * 1024 * 1024))
