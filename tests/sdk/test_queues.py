"""Queue storage: the data plane, with Shared Key, Entra ID and SAS."""

import datetime
import time

import pytest
from azure.core.exceptions import ClientAuthenticationError, HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.storage.queue import QueueClient, QueueSasPermissions, QueueServiceClient, generate_queue_sas


@pytest.mark.parametrize("auth", ["shared-key", "entra"])
def test_queues_and_metadata(credential, account, auth):
    service = QueueServiceClient(account.queue_url, credential=account.key if auth == "shared-key" else credential)
    queue = service.create_queue("jobs", metadata={"team": "billing"})
    # The service answers 204 for the same queue again, 409 for a different
    # one; the SDK raises for both.
    with pytest.raises(ResourceExistsError):
        service.create_queue("jobs", metadata={"team": "billing"})
    with pytest.raises(ResourceExistsError):
        service.create_queue("jobs", metadata={"team": "other"})
    assert [(q.name, q.metadata) for q in service.list_queues(include_metadata=True)] == [("jobs", {"team": "billing"})]
    queue.set_queue_metadata({"team": "ops"})
    assert queue.get_queue_properties().metadata == {"team": "ops"}
    service.delete_queue("jobs")
    with pytest.raises(ResourceNotFoundError):
        queue.get_queue_properties()


def test_messages(account):
    queue = QueueClient(account.queue_url, "work", credential=account.key)
    queue.create_queue()
    queue.send_message("first")
    queue.send_message("hidden", visibility_timeout=60)
    queue.send_message("<b>&</b>")
    assert queue.get_queue_properties().approximate_message_count == 3
    assert [m.content for m in queue.peek_messages(max_messages=10)] == ["first", "<b>&</b>"]

    received = list(queue.receive_messages(messages_per_page=10, visibility_timeout=30))
    assert [(m.content, m.dequeue_count) for m in received] == [("first", 1), ("<b>&</b>", 1)]
    assert queue.peek_messages(max_messages=10) == [], "received messages are invisible"

    queue.update_message(received[0], content="first, again", visibility_timeout=0)
    [again] = list(queue.receive_messages())
    assert (again.content, again.dequeue_count) == ("first, again", 2)
    queue.delete_message(again)
    with pytest.raises(HttpResponseError):
        queue.delete_message(received[1].id, pop_receipt="not-the-receipt")
    queue.delete_message(received[1])

    queue.send_message("short-lived", time_to_live=1)
    time.sleep(2)
    assert queue.peek_messages(max_messages=10) == [], "expired messages are gone"
    queue.send_message("x")
    queue.clear_messages()
    assert queue.get_queue_properties().approximate_message_count == 0, "clearing removes invisible messages too"


def test_sas_and_keys(storage, group, account):
    QueueClient(account.queue_url, "jobs", credential=account.key).create_queue()
    expiry = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=1)
    add_only = generate_queue_sas(account.name, "jobs", account.key, permission=QueueSasPermissions(add=True), expiry=expiry)
    sender = QueueClient(account.queue_url, "jobs", credential=add_only)
    sender.send_message("hello")
    with pytest.raises(HttpResponseError):
        list(sender.receive_messages())
    assert [q.name for q in storage.queue.list(group, account.name)] == ["jobs"]
    assert storage.queue.get(group, account.name, "jobs").approximate_message_count == 1
    forged = QueueServiceClient(account.queue_url, credential={"account_name": account.name, "account_key": "A" * 86 + "=="})
    with pytest.raises(ClientAuthenticationError):
        forged.create_queue("nope")


def test_access_policies(account):
    from azure.storage.queue import AccessPolicy

    queue = QueueClient(account.queue_url, "guarded", credential=account.key)
    queue.create_queue()
    expiry = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=1)
    queue.set_queue_access_policy({"senders": AccessPolicy(permission="a", expiry=expiry)})
    assert list(queue.get_queue_access_policy()) == ["senders"]
    queue.set_queue_access_policy({})
    assert queue.get_queue_access_policy() == {}
