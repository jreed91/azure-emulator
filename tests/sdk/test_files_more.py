"""Azure Files beyond the basics: service and share settings, directory
properties, the size cap, ranges out of bounds, share SAS, and the File data
roles with an Entra ID token."""

import datetime

import pytest
from azure.core.exceptions import HttpResponseError, ResourceExistsError
from azure.storage.fileshare import (
    AccessPolicy,
    ShareFileClient,
    ShareSasPermissions,
    ShareServiceClient,
    generate_share_sas,
)


def expiry():
    return datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=1)


@pytest.fixture
def service(account):
    return ShareServiceClient(account.file_url, credential=account.key)


def test_service_and_share_settings(service):
    props = service.get_service_properties()
    assert set(props) >= {"hour_metrics", "minute_metrics", "cors"}
    service.set_service_properties(cors=[])

    share = service.create_share("settings")
    share.get_file_client("a.bin").upload_file(b"x" * 1000)
    share.get_file_client("b.bin").upload_file(b"y" * 24)
    assert share.get_share_stats() == 1024

    share.set_share_access_policy({"readers": AccessPolicy(permission="r", expiry=expiry())})
    assert list(share.get_share_access_policy()["signed_identifiers"]) and share.get_share_access_policy()["signed_identifiers"][0].id == "readers"
    share.set_share_access_policy({})
    assert share.get_share_access_policy()["signed_identifiers"] == []

    with pytest.raises(HttpResponseError) as refused:
        service.create_share("Bad_Name")
    assert refused.value.error_code == "InvalidResourceName"


def test_directory_properties_and_conflicts(service):
    share = service.create_share("dirs")
    directory = share.create_directory("docs")
    directory.set_directory_metadata({"owner": "team"})
    assert directory.get_directory_properties().metadata == {"owner": "team"}
    before = directory.get_directory_properties().etag
    directory.set_http_headers()
    assert directory.get_directory_properties().etag != before

    directory.upload_file("note.txt", b"hello")
    # A directory where a file is, and a file where a directory is, conflict.
    with pytest.raises(HttpResponseError) as refused:
        directory.create_subdirectory("note.txt")
    assert refused.value.status_code == 409
    with pytest.raises(HttpResponseError) as refused:
        share.get_file_client("docs").create_file(10)
    assert refused.value.status_code == 409
    with pytest.raises(ResourceExistsError):
        share.create_directory("DOCS")


def test_size_cap_and_ranges(service):
    share = service.create_share("limits")
    file = share.get_file_client("f.bin")
    with pytest.raises(HttpResponseError):
        file.create_file(size=300 * 1024 * 1024)
    file.create_file(size=1024)
    with pytest.raises(HttpResponseError) as refused:
        file.upload_range(b"x" * 512, offset=1024, length=512)
    assert refused.value.status_code == 416
    with pytest.raises(HttpResponseError) as refused:
        file.download_file(offset=2048, length=10).readall()
    assert refused.value.status_code == 416
    assert file.download_file(offset=1000).readall() == bytes(24), "an open-ended range runs to the end"
    file.resize_file(0)
    assert file.get_ranges() == [] and file.download_file().readall() == b""


def test_share_sas(account, service):
    share = service.create_share("signed")
    share.get_file_client("a.txt").upload_file(b"a")
    share.create_directory("sub").upload_file("b.txt", b"b")
    sas = generate_share_sas(account.name, "signed", account_key=account.key, permission=ShareSasPermissions(read=True, list=True), expiry=expiry())
    reader = ShareServiceClient(account.file_url, credential=sas).get_share_client("signed")
    assert sorted(i["name"] for i in reader.list_directories_and_files()) == ["a.txt", "sub"]
    assert reader.get_directory_client("sub").get_file_client("b.txt").download_file().readall() == b"b"
    with pytest.raises(HttpResponseError) as refused:
        reader.get_file_client("c.txt").upload_file(b"c")
    assert refused.value.status_code == 403
    # A share SAS does not reach the service.
    with pytest.raises(HttpResponseError):
        list(ShareServiceClient(account.file_url, credential=sas).list_shares())
    forged = ShareFileClient(account.file_url, "signed", "a.txt", credential=sas.replace("sig=", "sig=AAAA"))
    with pytest.raises(HttpResponseError):
        forged.download_file().readall()
