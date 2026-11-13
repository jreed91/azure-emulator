"""Azure Files: shares, directories and files through the file share SDK."""

import datetime

import pytest
from azure.core.exceptions import ClientAuthenticationError, HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.storage.fileshare import (
    AccountSasPermissions,
    ContentSettings,
    ResourceTypes,
    ShareFileClient,
    ShareServiceClient,
    generate_account_sas,
    generate_file_sas,
    FileSasPermissions,
)


@pytest.fixture
def service(account):
    return ShareServiceClient(account.file_url, credential=account.key)


def test_shares(service, storage, group, account):
    share = service.create_share("docs", metadata={"team": "a"}, quota=10)
    with pytest.raises(ResourceExistsError):
        share.create_share()
    props = share.get_share_properties()
    assert (props.quota, props.metadata) == (10, {"team": "a"})
    share.set_share_quota(20)
    share.set_share_metadata({"team": "b"})
    assert [(s.name, s.quota, s.metadata) for s in service.list_shares(include_metadata=True)] == [("docs", 20, {"team": "b"})]

    # The same shares on Resource Manager, both ways.
    assert [s.name for s in storage.file_shares.list(group, account.name)] == ["docs"]
    storage.file_shares.create(group, account.name, "from-arm", {})
    assert sorted(s.name for s in service.list_shares()) == ["docs", "from-arm"]
    service.get_share_client("from-arm").create_directory("made-here")
    storage.file_shares.delete(group, account.name, "from-arm")
    service.delete_share("docs")
    assert list(service.list_shares()) == []
    with pytest.raises(ResourceNotFoundError):
        share.get_share_properties()


def test_directories_and_files(service):
    share = service.create_share("work")
    reports = share.create_directory("Reports", metadata={"kind": "monthly"})
    with pytest.raises(ResourceExistsError):
        share.create_directory("reports")  # names are case-insensitive
    reports.create_subdirectory("2024")
    data = bytes(range(256)) * 40  # 10 KiB
    reports.upload_file("summary.csv", data, content_settings=ContentSettings(content_type="text/csv"), metadata={"v": "1"})

    listed = [(item["name"], item["is_directory"]) for item in reports.list_directories_and_files()]
    assert listed == [("2024", True), ("summary.csv", False)]
    assert [item["name"] for item in share.list_directories_and_files()] == ["Reports"]
    assert [item["name"] for item in reports.list_directories_and_files(name_starts_with="sum")] == ["summary.csv"]

    file = reports.get_file_client("summary.csv")
    props = file.get_file_properties()
    assert (props.size, props.content_settings.content_type, props.metadata) == (len(data), "text/csv", {"v": "1"})
    assert file.download_file().readall() == data
    assert file.download_file(offset=100, length=50).readall() == data[100:150]
    assert reports.get_directory_properties().metadata == {"kind": "monthly"}

    with pytest.raises(HttpResponseError) as refused:
        reports.delete_directory()
    assert refused.value.error_code == "DirectoryNotEmpty"
    with pytest.raises(ResourceNotFoundError):
        share.get_directory_client("missing/child").create_directory()


def test_ranges_resize_and_headers(service):
    share = service.create_share("disk")
    file = share.get_file_client("image.bin")
    file.create_file(size=4096)
    assert file.download_file().readall() == bytes(4096)
    file.upload_range(b"A" * 1024, offset=0, length=1024)
    file.upload_range(b"B" * 512, offset=2048, length=512)
    assert [(r["start"], r["end"]) for r in file.get_ranges()] == [(0, 1023), (2048, 2559)]
    file.clear_range(offset=512, length=512)
    assert [(r["start"], r["end"]) for r in file.get_ranges()] == [(0, 511), (2048, 2559)]
    content = file.download_file().readall()
    assert content[:512] == b"A" * 512 and content[512:1024] == bytes(512) and content[2048:2560] == b"B" * 512

    file.set_http_headers(ContentSettings(content_type="application/x-disk", cache_control="no-cache"))
    file.resize_file(1024)
    props = file.get_file_properties()
    assert (props.size, props.content_settings.content_type, props.content_settings.cache_control) == (1024, "application/x-disk", "no-cache")
    file.set_file_metadata({"m": "1"})
    assert file.get_file_properties().metadata == {"m": "1"}
    file.delete_file()
    with pytest.raises(ResourceNotFoundError):
        file.get_file_properties()


def test_sas_and_forged_keys(service, account):
    service.create_share("shared").get_file_client("note.txt").upload_file(b"hello")
    expiry = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=1)
    read_only = generate_file_sas(account.name, "shared", ["note.txt"], account_key=account.key, permission=FileSasPermissions(read=True), expiry=expiry)
    reader = ShareFileClient(account.file_url, "shared", "note.txt", credential=read_only)
    assert reader.download_file().readall() == b"hello"
    with pytest.raises(HttpResponseError):
        reader.upload_file(b"changed")

    account_sas = generate_account_sas(account.name, account.key, ResourceTypes(service=True, container=True, object=True), AccountSasPermissions(read=True, list=True), expiry)
    assert [s.name for s in ShareServiceClient(account.file_url, credential=account_sas).list_shares()] == ["shared"]

    forged = ShareServiceClient(account.file_url, credential={"account_name": account.name, "account_key": "A" * 86 + "=="})
    with pytest.raises(ClientAuthenticationError):
        forged.create_share("nope")
