"""Event Hubs: namespaces, hubs and consumer groups on Resource Manager, and events over AMQP."""

import threading
import time

import pytest
from azure.core.exceptions import HttpResponseError
from azure.eventhub import EventData, EventHubConsumerClient, EventHubProducerClient

from conftest import unique


@pytest.fixture
def namespace(eventhub, group):
    return eventhub.namespaces.begin_create_or_update(group, unique("eh-"), {"location": "eastus", "sku": {"name": "Standard"}}).result()


@pytest.fixture
def connection(eventhub, group, namespace):
    eventhub.event_hubs.create_or_update(group, namespace.name, "telemetry", {"properties": {"partitionCount": 4}})
    return eventhub.namespaces.list_keys(group, namespace.name, "RootManageSharedAccessKey").primary_connection_string


def receive(connection, count, **options):
    """Events from every partition (or the one named) until `count` arrive."""
    got = []
    done = threading.Event()
    client = EventHubConsumerClient.from_connection_string(connection, consumer_group=options.pop("consumer_group", "$Default"), eventhub_name="telemetry")

    def on_event(context, event):
        if event is not None:
            got.append((context.partition_id, event))
            if len(got) >= count:
                done.set()

    thread = threading.Thread(target=lambda: client.receive(on_event=on_event, starting_position="-1", **options), daemon=True)
    thread.start()
    done.wait(10)
    client.close()
    thread.join(5)
    return got


def test_namespace_hub_and_consumer_groups(eventhub, group, namespace):
    assert namespace.sku.name == "Standard"
    hub = eventhub.event_hubs.create_or_update(group, namespace.name, "orders", {"properties": {"partitionCount": 3, "messageRetentionInDays": 2}})
    assert hub.partition_ids == ["0", "1", "2"]
    assert [g.name for g in eventhub.consumer_groups.list_by_event_hub(group, namespace.name, "orders")] == ["$Default"]
    eventhub.consumer_groups.create_or_update(group, namespace.name, "orders", "audit", {})
    with pytest.raises(HttpResponseError):
        eventhub.event_hubs.create_or_update(group, namespace.name, "orders", {"properties": {"partitionCount": 5}})
    with pytest.raises(HttpResponseError):
        eventhub.event_hubs.create_or_update(group, namespace.name, "toolong", {"properties": {"messageRetentionInDays": 30}})


def test_send_and_receive(connection):
    with EventHubProducerClient.from_connection_string(connection, eventhub_name="telemetry") as producer:
        properties = producer.get_eventhub_properties()
        assert properties["partition_ids"] == ["0", "1", "2", "3"]
        batch = producer.create_batch(partition_key="device-1")
        batch.add(EventData("k1"))
        batch.add(EventData("k2"))
        producer.send_batch(batch)
        batch = producer.create_batch(partition_id="3")
        batch.add(EventData("p3"))
        producer.send_batch(batch)
        partition = producer.get_partition_properties("3")
        assert (partition["is_empty"], partition["last_enqueued_sequence_number"]) == (False, 0)

    got = receive(connection, 3)
    bodies = sorted(event.body_as_str() for _, event in got)
    assert bodies == ["k1", "k2", "p3"]
    keyed = [(pid, event) for pid, event in got if event.partition_key == b"device-1"]
    assert len({pid for pid, _ in keyed}) == 1, "one partition key, one partition"
    assert [event.sequence_number for _, event in keyed] == [0, 1]


def test_positions_owner_levels_and_live_events(connection):
    with EventHubProducerClient.from_connection_string(connection, eventhub_name="telemetry") as producer:
        for body in ["a", "b", "c"]:
            batch = producer.create_batch(partition_id="0")
            batch.add(EventData(body))
            producer.send_batch(batch)

    got = []
    client = EventHubConsumerClient.from_connection_string(connection, consumer_group="$Default", eventhub_name="telemetry")
    thread = threading.Thread(
        target=lambda: client.receive(
            on_event=lambda c, e: e is not None and got.append(e.body_as_str()),
            partition_id="0",
            starting_position=1,
            starting_position_inclusive=True,
            owner_level=1,
        ),
        daemon=True,
    )
    thread.start()
    time.sleep(3)
    with EventHubProducerClient.from_connection_string(connection, eventhub_name="telemetry") as producer:
        batch = producer.create_batch(partition_id="0")
        batch.add(EventData("live"))
        producer.send_batch(batch)
    time.sleep(3)
    client.close()
    thread.join(5)
    assert got == ["b", "c", "live"]


def test_wrong_key_is_refused(connection):
    forged = connection.replace(connection.split("SharedAccessKey=")[1].split(";")[0], "A" * 43 + "=")
    with EventHubProducerClient.from_connection_string(forged, eventhub_name="telemetry", retry_total=0) as producer:
        with pytest.raises(Exception):
            producer.send_batch([EventData("nope")])
