"""Blob behaviour the emulator does not match yet, asserted as Azure behaves.
Each test is a `known_gap`: it fails today and counts against the Blob score."""

import datetime

import pytest
from azure.core.exceptions import HttpResponseError
from azure.storage.blob import (
    AccessPolicy,
    BlobClient,
    BlobSasPermissions,
    BlobServiceClient,
    ContainerSasPermissions,
    generate_blob_sas,
    generate_container_sas,
)


def later(hours=1):
    return datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=hours)


@pytest.fixture
def container(account):
    container = BlobServiceClient(account.blob_url, credential=account.key).get_container_client("gaps")
    container.create_container()
    container.upload_blob("a.txt", b"data")
    return container


@pytest.mark.known_gap("shared access signatures that name a stored access policy are refused with 501")
def test_sas_from_a_stored_access_policy(account, container):
    container.set_container_access_policy({"readers": AccessPolicy(permission=ContainerSasPermissions(read=True), expiry=later())})
    sas = generate_container_sas(account.name, "gaps", account_key=account.key, policy_id="readers")
    blob = BlobClient(account.blob_url, "gaps", "a.txt", credential=sas)
    assert blob.download_blob().readall() == b"data"
    # The policy, not the token, carries the permissions: it does not allow writes.
    with pytest.raises(HttpResponseError) as refused:
        blob.upload_blob(b"new", overwrite=True)
    assert refused.value.status_code == 403


@pytest.mark.known_gap("there is no user delegation key, so user delegation SAS tokens cannot be made or verified")
def test_user_delegation_sas(account, container, credential, storage, group, authorization, emulator):
    service = BlobServiceClient(account.blob_url, credential=credential)
    key = service.get_user_delegation_key(datetime.datetime.now(datetime.timezone.utc), later())
    assert key.value
    sas = generate_blob_sas(account.name, "gaps", "a.txt", user_delegation_key=key, permission=BlobSasPermissions(read=True), expiry=later())
    assert BlobClient(account.blob_url, "gaps", "a.txt", credential=sas).download_blob().readall() == b"data"


@pytest.mark.known_gap("a shared access signature's IP restriction is not enforced")
def test_sas_ip_restriction(account, container):
    sas = generate_blob_sas(
        account.name, "gaps", "a.txt", account_key=account.key, permission=BlobSasPermissions(read=True), expiry=later(), ip="203.0.113.7"
    )
    with pytest.raises(HttpResponseError) as refused:
        BlobClient(account.blob_url, "gaps", "a.txt", credential=sas).download_blob().readall()
    assert refused.value.status_code == 403
    assert refused.value.error_code == "AuthorizationSourceIPMismatch"


@pytest.mark.known_gap("blob versions are not kept")
def test_blob_versions(account, container, storage, group):
    storage.blob_services.set_service_properties(group, account.name, {"is_versioning_enabled": True})
    first = container.upload_blob("versioned.txt", b"v1")
    first_version = first.get_blob_properties().version_id
    assert first_version
    container.upload_blob("versioned.txt", b"v2", overwrite=True)
    versions = [b for b in container.list_blobs(name_starts_with="versioned.txt", include=["versions"])]
    assert len(versions) == 2
    assert container.get_blob_client("versioned.txt").download_blob(version_id=first_version).readall() == b"v1"
