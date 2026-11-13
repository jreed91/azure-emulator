"""Append and page blobs through the Blob SDK."""

import pytest
from azure.core import MatchConditions
from azure.core.exceptions import HttpResponseError, ResourceModifiedError
from azure.storage.blob import BlobServiceClient, BlobType, SequenceNumberAction


@pytest.fixture
def container(account):
    container = BlobServiceClient(account.blob_url, credential=account.key).get_container_client("types")
    container.create_container()
    return container


def test_append_blobs(container):
    log = container.get_blob_client("app.log")
    log.create_append_blob(metadata={"kind": "log"})
    assert log.append_block(b"line 1\n")["blob_append_offset"] == "0"
    result = log.append_block(b"line 2\n")
    assert (result["blob_append_offset"], result["blob_committed_block_count"]) == ("7", 2)
    assert log.download_blob().readall() == b"line 1\nline 2\n"
    props = log.get_blob_properties()
    assert (props.blob_type, props.append_blob_committed_block_count, props.metadata) == (BlobType.AppendBlob, 2, {"kind": "log"})

    with pytest.raises(HttpResponseError) as refused:
        log.append_block(b"x", appendpos_condition=0)
    assert refused.value.error_code == "AppendPositionConditionNotMet"
    with pytest.raises(HttpResponseError) as refused:
        log.append_block(b"too much", maxsize_condition=16)
    assert refused.value.error_code == "MaxBlobSizeConditionNotMet"
    with pytest.raises(HttpResponseError) as refused:
        log.stage_block("YQ==", b"block")
    assert refused.value.error_code == "InvalidBlobType"

    log.seal_append_blob()
    assert log.get_blob_properties().is_append_blob_sealed
    with pytest.raises(HttpResponseError) as refused:
        log.append_block(b"after")
    assert refused.value.error_code == "BlobIsSealed"

    # Uploading through the SDK's append path: create, then append the data.
    uploaded = container.upload_blob("uploaded.log", b"a" * 10_000, blob_type=BlobType.AppendBlob)
    assert uploaded.download_blob().readall() == b"a" * 10_000
    assert [(b.name, b.blob_type) for b in container.list_blobs()] == [("app.log", BlobType.AppendBlob), ("uploaded.log", BlobType.AppendBlob)]


def test_page_blobs(container):
    disk = container.get_blob_client("disk.vhd")
    disk.create_page_blob(size=4096, sequence_number=7)
    props = disk.get_blob_properties()
    assert (props.blob_type, props.size, props.page_blob_sequence_number) == (BlobType.PageBlob, 4096, 7)
    assert disk.download_blob().readall() == bytes(4096)

    disk.upload_page(b"A" * 512, offset=0, length=512)
    disk.upload_page(b"B" * 1024, offset=1024, length=1024)
    disk.upload_page(b"C" * 512, offset=2048, length=512)
    ranges, cleared = disk.get_page_ranges()
    assert [(r["start"], r["end"]) for r in ranges] == [(0, 511), (1024, 2559)]
    data = disk.download_blob().readall()
    assert data[:512] == b"A" * 512 and data[1024:2048] == b"B" * 1024 and data[512:1024] == bytes(512)

    disk.clear_page(offset=1024, length=512)
    assert [(r["start"], r["end"]) for r in disk.get_page_ranges()[0]] == [(0, 511), (1536, 2559)]
    assert disk.download_blob(offset=1024, length=512).readall() == bytes(512)

    with pytest.raises(HttpResponseError) as refused:
        disk.upload_page(b"x" * 512, offset=8192, length=512)  # past the end
    assert refused.value.error_code == "InvalidPageRange"
    with pytest.raises(HttpResponseError) as refused:
        disk.upload_page(b"D" * 512, offset=0, length=512, if_sequence_number_lt=7)
    assert refused.value.error_code == "SequenceNumberConditionNotMet"

    assert disk.set_sequence_number(SequenceNumberAction.Increment)["blob_sequence_number"] == 8
    assert disk.set_sequence_number(SequenceNumberAction.Max, 5)["blob_sequence_number"] == 8
    assert disk.set_sequence_number(SequenceNumberAction.Update, 42)["blob_sequence_number"] == 42

    disk.set_http_headers()  # content settings stay a separate operation from resizing
    disk.resize_blob(2048)
    assert disk.get_blob_properties().size == 2048
    assert [(r["start"], r["end"]) for r in disk.get_page_ranges()[0]] == [(0, 511), (1536, 2047)]

    with pytest.raises(HttpResponseError) as refused:
        disk.commit_block_list([])
    assert refused.value.error_code == "InvalidBlobType"
    with pytest.raises(ResourceModifiedError):
        disk.upload_page(b"E" * 512, offset=0, length=512, etag='"0x0"', match_condition=MatchConditions.IfNotModified)

    snapshot = disk.create_snapshot()
    assert container.get_blob_client("disk.vhd", snapshot=snapshot["snapshot"]).get_blob_properties().blob_type == BlobType.PageBlob
