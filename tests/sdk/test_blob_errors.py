"""Blob refusals: missing containers and blobs, protected blobs, and tokens
for another audience."""

import pytest
from azure.core.exceptions import ClientAuthenticationError, HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.storage.blob import BlobServiceClient

from conftest import TENANT


def test_missing_containers_and_blobs(account):
    service = BlobServiceClient(account.blob_url, credential=account.key)
    missing = service.get_container_client("absent")
    with pytest.raises(ResourceNotFoundError) as refused:
        missing.upload_blob("a.txt", b"x")
    assert refused.value.error_code == "ContainerNotFound"
    with pytest.raises(ResourceNotFoundError):
        list(missing.list_blobs())
    with pytest.raises(ResourceNotFoundError):
        missing.delete_container()
    container = service.create_container("present")
    with pytest.raises(ResourceExistsError):
        service.create_container("present")
    with pytest.raises(ResourceNotFoundError) as refused:
        container.get_blob_client("nope").download_blob()
    assert refused.value.error_code == "BlobNotFound"
    with pytest.raises(ResourceNotFoundError):
        container.get_blob_client("nope").set_blob_metadata({"a": "b"})
    with pytest.raises(HttpResponseError) as refused:
        service.create_container("Bad_Name")
    assert refused.value.error_code == "InvalidResourceName"


def test_legal_hold_protects_updates(storage, group, account):
    container = BlobServiceClient(account.blob_url, credential=account.key).create_container("held")
    blob = container.upload_blob("record.txt", b"kept")
    storage.blob_containers.set_legal_hold(group, account.name, "held", {"tags": ["audit"]})
    for change in (lambda: blob.set_blob_metadata({"a": "b"}), lambda: blob.set_http_headers(), lambda: blob.upload_blob(b"new", overwrite=True)):
        with pytest.raises(HttpResponseError) as refused:
            change()
        assert refused.value.error_code == "BlobImmutableDueToLegalHold"
    storage.blob_containers.clear_legal_hold(group, account.name, "held", {"tags": ["audit"]})
    blob.set_blob_metadata({"a": "b"})


def test_tokens_for_another_audience_are_refused(emulator, account):
    from azure.identity import ClientSecretCredential

    credential = ClientSecretCredential(TENANT, "audience-test", "secret", authority=emulator.base, disable_instance_discovery=True)
    token = credential.get_token(emulator.base + "/.default")

    class Fixed:
        def get_token(self, *scopes, **kwargs):
            return token

    with pytest.raises(ClientAuthenticationError) as refused:
        BlobServiceClient(account.blob_url, credential=Fixed()).create_container("aud")
    assert refused.value.error_code == "InvalidAuthenticationInfo"
