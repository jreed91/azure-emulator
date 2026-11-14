"""Service Bus beyond the basics: the SQL filter language case by case, rule
actions on system properties, settling deferred messages, dead-lettering on
expiry, and lost locks. Expected matches follow Service Bus SQL semantics: a
missing property IS NULL, and any other comparison with it is unknown."""

import datetime
import time

import pytest
from azure.mgmt.servicebus.models import Action, Rule, SqlFilter
from azure.servicebus import ServiceBusMessage, ServiceBusSubQueue
from azure.servicebus.exceptions import MessageLockLostError

from test_servicebus import _client, namespace  # noqa: F401  (fixture)

MESSAGES = [
    ServiceBusMessage(
        "first", message_id="order-1", subject="order", content_type="application/json", correlation_id="c1",
        application_properties={"quantity": 12, "price": 10.0, "color": "red"},
    ),
    ServiceBusMessage(
        "second", message_id="inv-2", subject="invoice", content_type="text/plain", to="dest",
        application_properties={"quantity": 2, "price": 100.5, "color": "blue", "flag": True},
    ),
    ServiceBusMessage("third", message_id="x-3", reply_to="back", application_properties={"quantity": -7, "color": "green", "name": "O'Hara"}),
]

# (SQL filter, message ids it matches)
FILTERS = [
    ("sys.Label = 'order'", ["order-1"]),
    ("sys.ContentType = 'text/plain'", ["inv-2"]),
    ("sys.MessageId LIKE 'order-%'", ["order-1"]),
    ("sys.CorrelationId = 'c1'", ["order-1"]),
    ("sys.To = 'dest'", ["inv-2"]),
    ("sys.ReplyTo = 'back'", ["x-3"]),
    ("quantity * price > 100", ["order-1", "inv-2"]),
    ("-quantity > 5", ["x-3"]),
    ("quantity + 3 = 15", ["order-1"]),
    ("quantity % 2 = 0", ["order-1", "inv-2"]),
    ("quantity / 2 = 6", ["order-1"]),
    ("color IN ('red', 'blue')", ["order-1", "inv-2"]),
    ("color NOT IN ('red')", ["inv-2", "x-3"]),
    ("EXISTS(flag)", ["inv-2"]),
    ("NOT EXISTS(price)", ["x-3"]),
    ("flag = TRUE", ["inv-2"]),
    ("price IS NULL", ["x-3"]),
    ("price IS NOT NULL", ["order-1", "inv-2"]),
    ("name = 'O''Hara'", ["x-3"]),
    ("color LIKE 'r_d'", ["order-1"]),
    ("color NOT LIKE 'r%'", ["inv-2", "x-3"]),
    ("color LIKE 'gr%' OR quantity < 0", ["x-3"]),
    ("quantity > 1 AND NOT (color = 'red')", ["inv-2"]),
    ("missing = 1", []),
    ("1 = 1", ["order-1", "inv-2", "x-3"]),
]


def test_sql_filter_language(servicebus, group, namespace):  # noqa: F811
    servicebus.topics.create_or_update(group, namespace.name, "events", {})
    for i, (expression, _) in enumerate(FILTERS):
        servicebus.subscriptions.create_or_update(group, namespace.name, "events", f"s{i}", {})
        servicebus.rules.delete(group, namespace.name, "events", f"s{i}", "$Default")
        servicebus.rules.create_or_update(group, namespace.name, "events", f"s{i}", "only", Rule(filter_type="SqlFilter", sql_filter=SqlFilter(sql_expression=expression)))
    with _client(servicebus, group, namespace) as client:
        with client.get_topic_sender("events") as sender:
            sender.send_messages(list(MESSAGES))
        wrong = []
        for i, (expression, expected) in enumerate(FILTERS):
            # Peeking answers at once, where receiving an empty subscription waits.
            with client.get_subscription_receiver("events", f"s{i}") as receiver:
                got = sorted(str(m.message_id) for m in receiver.peek_messages(max_message_count=10))
            if got != sorted(expected):
                wrong.append(f"{expression}: expected {sorted(expected)}, got {got}")
    assert not wrong, "\n".join(wrong)


def test_rule_actions_set_system_properties(servicebus, group, namespace):  # noqa: F811
    servicebus.topics.create_or_update(group, namespace.name, "routing", {})
    servicebus.subscriptions.create_or_update(group, namespace.name, "routing", "routed", {})
    servicebus.rules.delete(group, namespace.name, "routing", "routed", "$Default")
    servicebus.rules.create_or_update(
        group, namespace.name, "routing", "routed", "route",
        Rule(
            filter_type="SqlFilter",
            sql_filter=SqlFilter(sql_expression="sys.MessageId = 'order-1'"),
            action=Action(sql_expression="SET sys.Label = 'routed'; SET sys.To = 'there'; SET total = quantity * price; SET sys.CorrelationId = 'set'"),
        ),
    )
    with _client(servicebus, group, namespace) as client:
        with client.get_topic_sender("routing") as sender:
            sender.send_messages(list(MESSAGES))
        with client.get_subscription_receiver("routing", "routed", max_wait_time=2) as receiver:
            [message] = receiver.receive_messages(max_message_count=5, max_wait_time=2)
            assert (message.subject, message.to, message.correlation_id) == ("routed", "there", "set")
            assert message.application_properties[b"total"] == 120.0
            receiver.complete_message(message)


def test_settling_deferred_messages(servicebus, group, namespace):  # noqa: F811
    servicebus.queues.create_or_update(group, namespace.name, "later", {})
    with _client(servicebus, group, namespace) as client:
        with client.get_queue_sender("later") as sender:
            sender.send_messages([ServiceBusMessage(f"m{i}") for i in range(4)])
        with client.get_queue_receiver("later", max_wait_time=2) as receiver:
            received = []
            while len(received) < 4:
                batch = receiver.receive_messages(max_message_count=4, max_wait_time=2)
                assert batch, "every message arrives"
                received += batch
            for message in received:
                receiver.defer_message(message)
            numbers = [m.sequence_number for m in received]
            deferred = {str(m): m for m in receiver.receive_deferred_messages(numbers)}
            assert sorted(deferred) == ["m0", "m1", "m2", "m3"]
            # Settled through the management link, as deferred messages are.
            receiver.complete_message(deferred["m0"])
            receiver.dead_letter_message(deferred["m1"], reason="bad", error_description="cannot process")
            receiver.abandon_message(deferred["m2"])
            receiver.defer_message(deferred["m3"])
            # An abandoned or re-deferred message stays deferred, and the completed one is gone.
            again = sorted(str(m) for m in receiver.receive_deferred_messages(numbers[2:]))
            assert again == ["m2", "m3"]
            assert receiver.receive_messages(max_message_count=5, max_wait_time=1) == []
        with client.get_queue_receiver("later", sub_queue=ServiceBusSubQueue.DEAD_LETTER, max_wait_time=2) as dead:
            [letter] = dead.receive_messages(max_message_count=5, max_wait_time=2)
            assert (str(letter), letter.dead_letter_reason, letter.dead_letter_error_description) == ("m1", "bad", "cannot process")
            dead.complete_message(letter)


def test_expired_messages_are_dead_lettered(servicebus, group, namespace):  # noqa: F811
    servicebus.queues.create_or_update(group, namespace.name, "fleeting", {"properties": {"deadLetteringOnMessageExpiration": True}})
    servicebus.queues.create_or_update(group, namespace.name, "dropped", {})
    with _client(servicebus, group, namespace) as client:
        for queue in ["fleeting", "dropped"]:
            with client.get_queue_sender(queue) as sender:
                sender.send_messages(ServiceBusMessage("short", time_to_live=datetime.timedelta(seconds=1)))
        time.sleep(2)
        for queue in ["fleeting", "dropped"]:
            with client.get_queue_receiver(queue, max_wait_time=1) as receiver:
                assert receiver.receive_messages(max_message_count=5, max_wait_time=1) == []
        with client.get_queue_receiver("fleeting", sub_queue=ServiceBusSubQueue.DEAD_LETTER, max_wait_time=2) as dead:
            [letter] = dead.receive_messages(max_message_count=5, max_wait_time=2)
            assert (str(letter), letter.dead_letter_reason) == ("short", "TTLExpiredException")
        with client.get_queue_receiver("dropped", sub_queue=ServiceBusSubQueue.DEAD_LETTER, max_wait_time=1) as dead:
            assert dead.receive_messages(max_message_count=5, max_wait_time=1) == []


def test_lost_lock(servicebus, group, namespace):  # noqa: F811
    servicebus.queues.create_or_update(group, namespace.name, "brief", {"properties": {"lockDuration": "PT5S"}})
    with _client(servicebus, group, namespace) as client:
        with client.get_queue_sender("brief") as sender:
            sender.send_messages(ServiceBusMessage("slow"))
        with client.get_queue_receiver("brief", max_wait_time=2) as receiver:
            [message] = receiver.receive_messages(max_message_count=1, max_wait_time=2)
            time.sleep(6)
            with pytest.raises(MessageLockLostError):
                receiver.complete_message(message)
            # The message is delivered again, counting the earlier delivery.
            [again] = receiver.receive_messages(max_message_count=1, max_wait_time=2)
            assert (str(again), again.delivery_count) == ("slow", 1)
            receiver.complete_message(again)
