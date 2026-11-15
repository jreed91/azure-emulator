"""Azure Files behaviour the emulator does not match yet, asserted as Azure
behaves. Each test is a `known_gap`: it fails today and counts against the
Azure Files score."""

import datetime
import time

import pytest
from azure.core.exceptions import HttpResponseError
from azure.storage.fileshare import ShareFileClient, ShareSasPermissions, ShareServiceClient, generate_share_sas


@pytest.fixture
def share(account):
    share = ShareServiceClient(account.file_url, credential=account.key).create_share("gaps")
    share.get_file_client("a.txt").upload_file(b"data")
    return share


@pytest.mark.known_gap("share snapshots are not implemented")
def test_share_snapshot(account, share):
    snapshot = share.create_snapshot()["snapshot"]
    share.get_file_client("a.txt").upload_file(b"changed")
    old = ShareFileClient(account.file_url, "gaps", "a.txt", snapshot=snapshot, credential=account.key)
    assert old.download_file().readall() == b"data"


@pytest.mark.known_gap("copying a file is not implemented")
def test_file_copy(account, share):
    expiry = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=1)
    sas = generate_share_sas(account.name, "gaps", account_key=account.key, permission=ShareSasPermissions(read=True), expiry=expiry)
    target = share.get_file_client("copy.txt")
    target.start_copy_from_url(f"{account.file_url.rstrip('/')}/gaps/a.txt?{sas}")
    deadline = time.monotonic() + 30
    while target.get_file_properties().copy.status == "pending" and time.monotonic() < deadline:
        time.sleep(0.5)
    assert target.get_file_properties().copy.status == "success"
    assert target.download_file().readall() == b"data"


@pytest.mark.known_gap("file leases are not implemented")
def test_file_lease(share):
    file = share.get_file_client("a.txt")
    lease = file.acquire_lease()
    with pytest.raises(HttpResponseError) as refused:
        file.upload_file(b"changed")
    assert refused.value.error_code == "LeaseIdMissing"
    lease.release()
    file.upload_file(b"changed")
