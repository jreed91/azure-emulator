"""Event Hubs beyond the happy path: starting positions, consumer groups,
epochs, event metadata, hub and partition properties, and the refusals."""

import datetime
import threading
import time

import pytest
from azure.eventhub import EventData, EventHubConsumerClient, EventHubProducerClient

from test_eventhubs import connection, namespace  # noqa: F401  (fixtures)


def send(connection, bodies, partition_id="0", **properties):  # noqa: F811
    with EventHubProducerClient.from_connection_string(connection, eventhub_name="telemetry") as producer:
        for body in bodies:
            batch = producer.create_batch(partition_id=partition_id)
            event = EventData(body)
            event.properties = properties or None
            batch.add(event)
            producer.send_batch(batch)


def collect(connection, seconds=3, **options):  # noqa: F811
    """What a receiver gets in `seconds`, and the errors it reports."""
    events, errors = [], []
    client = EventHubConsumerClient.from_connection_string(
        connection, consumer_group=options.pop("consumer_group", "$Default"), eventhub_name="telemetry", retry_total=0
    )
    thread = threading.Thread(
        target=lambda: client.receive(
            on_event=lambda c, e: e is not None and events.append(e),
            on_error=lambda c, error: errors.append(error),
            **options,
        ),
        daemon=True,
    )
    thread.start()
    time.sleep(seconds)
    client.close()
    thread.join(5)
    return events, errors


def test_starting_positions(connection):  # noqa: F811
    send(connection, ["a", "b", "c"])
    after_sequence, _ = collect(connection, partition_id="0", starting_position=1)
    assert [e.body_as_str() for e in after_sequence] == ["c"], "a sequence number is exclusive by default"
    time.sleep(1.1)
    cutoff = datetime.datetime.now(datetime.timezone.utc)
    time.sleep(1.1)
    send(connection, ["d"])
    after_time, _ = collect(connection, partition_id="0", starting_position=cutoff)
    assert [e.body_as_str() for e in after_time] == ["d"]
    everything, _ = collect(connection, partition_id="0", starting_position="-1")
    assert [e.body_as_str() for e in everything] == ["a", "b", "c", "d"]
    assert [e.sequence_number for e in everything] == [0, 1, 2, 3]
    latest, _ = collect(connection, partition_id="0", starting_position="@latest", seconds=2)
    assert latest == []


def test_consumer_groups(eventhub, group, namespace, connection):  # noqa: F811
    eventhub.consumer_groups.create_or_update(group, namespace.name, "telemetry", "analytics", {})
    send(connection, ["shared"])
    default, _ = collect(connection, partition_id="0", starting_position="-1")
    analytics, _ = collect(connection, partition_id="0", starting_position="-1", consumer_group="analytics")
    assert [e.body_as_str() for e in default] == [e.body_as_str() for e in analytics] == ["shared"]
    _, errors = collect(connection, partition_id="0", starting_position="-1", consumer_group="missing", seconds=2)
    assert errors, "a consumer group that does not exist is refused"


def test_epochs(connection):  # noqa: F811
    send(connection, ["e"])
    results = {}

    def hold(name, **options):
        results[name] = collect(connection, partition_id="0", starting_position="-1", seconds=4, **options)

    owner = threading.Thread(target=hold, args=("owner",), kwargs={"owner_level": 5})
    owner.start()
    time.sleep(1.5)
    lower = threading.Thread(target=hold, args=("lower",), kwargs={"owner_level": 1})
    plain = threading.Thread(target=hold, args=("plain",))
    lower.start()
    plain.start()
    for thread in (owner, lower, plain):
        thread.join(15)
    assert [e.body_as_str() for e in results["owner"][0]] == ["e"]
    assert results["lower"][1], "a lower epoch is refused"
    assert results["plain"][1], "a receiver without an epoch is refused while one is held"
    assert results["lower"][0] == results["plain"][0] == []


def test_event_metadata_and_properties(connection):  # noqa: F811
    with EventHubProducerClient.from_connection_string(connection, eventhub_name="telemetry") as producer:
        hub = producer.get_eventhub_properties()
        assert (hub["eventhub_name"], hub["partition_ids"]) == ("telemetry", ["0", "1", "2", "3"])
        empty = producer.get_partition_properties("2")
        assert (empty["is_empty"], empty["last_enqueued_sequence_number"]) == (True, -1)
        event = EventData(b"\x00\x01binary")
        event.properties = {"kind": "reading", "n": 7}
        event.content_type = "application/octet-stream"
        event.correlation_id = "corr-1"
        event.message_id = "msg-1"
        batch = producer.create_batch(partition_id="1")
        batch.add(event)
        producer.send_batch(batch)
        filled = producer.get_partition_properties("1")
        assert (filled["is_empty"], filled["last_enqueued_sequence_number"], filled["beginning_sequence_number"]) == (False, 0, 0)

    [got], _ = collect(connection, partition_id="1", starting_position="-1")
    assert b"".join(got.body) == b"\x00\x01binary"
    assert got.properties == {b"kind": b"reading", b"n": 7}
    assert (got.content_type, got.correlation_id, got.message_id) == ("application/octet-stream", "corr-1", "msg-1")
    assert got.enqueued_time is not None and got.offset is not None


def test_refusals(connection):  # noqa: F811
    with EventHubProducerClient.from_connection_string(connection, eventhub_name="telemetry", retry_total=0) as producer:
        with pytest.raises(Exception) as missing:
            producer.send_batch([EventData("x")], partition_id="9")
        assert "9" in str(missing.value)
        with pytest.raises(Exception):
            producer.send_batch([EventData("x" * (1024 * 1024 + 1))])
        with pytest.raises(Exception):
            producer.get_partition_properties("9")
    with EventHubProducerClient.from_connection_string(connection, eventhub_name="no-such-hub", retry_total=0) as producer:
        with pytest.raises(Exception):
            producer.get_eventhub_properties()
