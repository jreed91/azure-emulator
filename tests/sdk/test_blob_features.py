"""Blob leases, snapshots, copies and conditional headers."""

import datetime

import pytest
from azure.core import MatchConditions
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceModifiedError, ResourceNotFoundError
from azure.storage.blob import BlobSasPermissions, BlobServiceClient, generate_blob_sas

from conftest import unique


@pytest.fixture
def container(account):
    container = BlobServiceClient(account.blob_url, credential=account.key).get_container_client("features")
    container.create_container()
    return container


def test_blob_leases(container):
    blob = container.upload_blob("leased.txt", b"v1")
    lease = blob.acquire_lease(lease_duration=15)
    props = blob.get_blob_properties()
    assert (props.lease.status, props.lease.state, props.lease.duration) == ("locked", "leased", "fixed")

    with pytest.raises(HttpResponseError) as refused:
        blob.upload_blob(b"v2", overwrite=True)
    assert refused.value.error_code == "LeaseIdMissing"
    with pytest.raises(HttpResponseError) as refused:
        blob.delete_blob()
    assert refused.value.error_code == "LeaseIdMissing"
    with pytest.raises(ResourceExistsError):
        blob.acquire_lease()

    blob.upload_blob(b"v2", overwrite=True, lease=lease)
    assert blob.download_blob(lease=lease).readall() == b"v2"
    lease.renew()
    new_id = "a8f4f84a-3c2f-4b67-9f4b-7b6d8f6c2a10"
    lease.change(new_id)
    assert lease.id == new_id
    blob.set_blob_metadata({"k": "v"}, lease=lease)
    lease.release()
    assert blob.get_blob_properties().lease.state == "available"

    infinite = blob.acquire_lease()
    assert blob.get_blob_properties().lease.duration == "infinite"
    assert infinite.break_lease() == 0
    props = blob.get_blob_properties()
    assert (props.lease.status, props.lease.state) == ("unlocked", "broken")
    blob.delete_blob()


def test_container_lease_on_the_data_plane(container):
    lease = container.acquire_lease(lease_duration=-1)
    assert container.get_container_properties().lease.state == "leased"
    with pytest.raises(HttpResponseError):
        container.delete_container()
    assert lease.break_lease(lease_break_period=10) == 10
    assert container.get_container_properties().lease.state == "breaking"
    lease.break_lease(lease_break_period=0)
    container.delete_container()


def test_snapshots(container):
    blob = container.upload_blob("doc.txt", b"first", metadata={"rev": "1"})
    snap = blob.create_snapshot()
    blob.upload_blob(b"second", overwrite=True)
    later = blob.create_snapshot(metadata={"rev": "2"})
    assert snap["snapshot"] < later["snapshot"]

    old = container.get_blob_client("doc.txt", snapshot=snap["snapshot"])
    assert old.download_blob().readall() == b"first"
    assert old.get_blob_properties().metadata == {"rev": "1"}
    assert container.get_blob_client("doc.txt", snapshot=later["snapshot"]).get_blob_properties().metadata == {"rev": "2"}

    listed = [(b.name, b.snapshot) for b in container.list_blobs(include=["snapshots"])]
    assert listed == [("doc.txt", snap["snapshot"]), ("doc.txt", later["snapshot"]), ("doc.txt", None)]
    assert [b.name for b in container.list_blobs()] == ["doc.txt"]

    with pytest.raises(HttpResponseError) as refused:
        blob.delete_blob()
    assert refused.value.error_code == "SnapshotsPresent"
    old.delete_blob()
    assert len(list(container.list_blobs(include=["snapshots"]))) == 2
    blob.delete_blob(delete_snapshots="only")
    assert blob.download_blob().readall() == b"second"
    blob.create_snapshot()
    blob.delete_blob(delete_snapshots="include")
    assert list(container.list_blobs(include=["snapshots"])) == []
    with pytest.raises(ResourceNotFoundError):
        old.get_blob_properties()


def test_copies(account, container):
    source = container.upload_blob("source.txt", b"payload", metadata={"origin": "a"})
    copy = container.get_blob_client("copy.txt")
    result = copy.start_copy_from_url(source.url)
    assert result["copy_status"] == "success"
    props = copy.get_blob_properties()
    assert (props.copy.status, props.copy.source, props.metadata) == ("success", source.url, {"origin": "a"})
    assert copy.download_blob().readall() == b"payload"

    synced = container.get_blob_client("sync.txt")
    synced.start_copy_from_url(source.url, metadata={"origin": "b"}, requires_sync=True)
    assert synced.get_blob_properties().metadata == {"origin": "b"}

    # A snapshot copied over its own blob restores it.
    snap = source.create_snapshot()
    source.upload_blob(b"changed", overwrite=True)
    source.start_copy_from_url(f"{source.url}?snapshot={snap['snapshot']}")
    assert source.download_blob().readall() == b"payload"

    with pytest.raises(HttpResponseError) as refused:
        copy.abort_copy(props.copy.id)
    assert refused.value.error_code == "NoPendingCopyOperation"
    with pytest.raises(ResourceNotFoundError):
        container.get_blob_client("x").start_copy_from_url(f"{container.url}/missing.txt")


def test_copy_from_another_account_needs_access(storage, group, account, container):
    name = unique("st")
    other = storage.storage_accounts.begin_create(group, name, {"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}}).result()
    key = storage.storage_accounts.list_keys(group, name)["keys"][0]["value"]
    target = BlobServiceClient(other.primary_endpoints.blob, credential=key).get_container_client("inbox")
    target.create_container()
    source = container.upload_blob("shared.txt", b"cross-account")

    with pytest.raises(HttpResponseError) as refused:
        target.get_blob_client("a.txt").upload_blob_from_url(source.url)
    assert refused.value.error_code == "CannotVerifyCopySource"

    expiry = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=1)
    sas = generate_blob_sas(account.name, "features", "shared.txt", account_key=account.key, permission=BlobSasPermissions(read=True), expiry=expiry)
    copied = target.get_blob_client("a.txt")
    copied.upload_blob_from_url(f"{source.url}?{sas}")
    assert copied.download_blob().readall() == b"cross-account"


def test_conditional_headers(container):
    blob = container.upload_blob("cond.txt", b"one")
    etag = blob.get_blob_properties().etag
    with pytest.raises(ResourceModifiedError):
        blob.upload_blob(b"two", overwrite=True, etag='"0x0"', match_condition=MatchConditions.IfNotModified)
    blob.upload_blob(b"two", overwrite=True, etag=etag, match_condition=MatchConditions.IfNotModified)
    with pytest.raises(ResourceModifiedError):
        blob.set_blob_metadata({"a": "b"}, etag=etag, match_condition=MatchConditions.IfNotModified)
    with pytest.raises(ResourceModifiedError):
        blob.delete_blob(etag=etag, match_condition=MatchConditions.IfNotModified)

    current = blob.get_blob_properties()
    # An unchanged blob read with If-None-Match is answered 304.
    with pytest.raises(HttpResponseError) as unchanged:
        blob.download_blob(etag=current.etag, match_condition=MatchConditions.IfModified).readall()
    assert unchanged.value.status_code == 304
    assert blob.download_blob(etag='"0x0"', match_condition=MatchConditions.IfModified).readall() == b"two"

    future = current.last_modified + datetime.timedelta(minutes=5)
    past = current.last_modified - datetime.timedelta(minutes=5)
    with pytest.raises(HttpResponseError):
        blob.download_blob(if_modified_since=future).readall()
    assert blob.download_blob(if_modified_since=past).readall() == b"two"
    with pytest.raises(ResourceModifiedError):
        blob.set_blob_metadata({"a": "b"}, if_unmodified_since=past)
    blob.set_blob_metadata({"a": "b"}, if_unmodified_since=future)

    # Source conditions on a copy.
    with pytest.raises(HttpResponseError) as refused:
        container.get_blob_client("c2.txt").start_copy_from_url(blob.url, source_etag='"0x0"', source_match_condition=MatchConditions.IfNotModified)
    assert refused.value.status_code == 412
