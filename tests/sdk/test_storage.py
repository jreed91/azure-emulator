"""Storage: the account on Resource Manager, the Blob data plane, and shared
access signatures."""

import datetime

import pytest
from azure.core import MatchConditions
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.storage.blob import (
    AccountSasPermissions,
    BlobClient,
    BlobSasPermissions,
    BlobServiceClient,
    ContainerClient,
    ContainerSasPermissions,
    ContentSettings,
    ResourceTypes,
    generate_account_sas,
    generate_blob_sas,
    generate_container_sas,
)


def expiry(hours=1):
    return datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=hours)


def test_account(storage, emulator, group, account):
    assert not storage.storage_accounts.check_name_availability({"name": "Bad_Name"}).name_available
    assert not storage.storage_accounts.check_name_availability({"name": account.name}).name_available
    assert account.blob_url == f"https://{account.name}.blob.core.azure.localhost:{emulator.port}/"
    assert account.name in [a.name for a in storage.storage_accounts.list()]
    assert account.name in [a.name for a in storage.storage_accounts.list_by_resource_group(group)]

    keys = storage.storage_accounts.list_keys(group, account.name)["keys"]
    regenerated = storage.storage_accounts.regenerate_key(group, account.name, {"keyName": "key1"})["keys"]
    assert regenerated[0]["value"] != keys[0]["value"]
    assert regenerated[1]["value"] == keys[1]["value"]


@pytest.mark.live
@pytest.mark.parametrize("auth", ["entra", "shared-key"])
def test_blob_round_trip(credential, account, auth):
    service = BlobServiceClient(account.blob_url, credential=credential if auth == "entra" else account.key)
    container = service.create_container("data")
    with pytest.raises(ResourceExistsError):
        service.create_container("data")

    blob = container.get_blob_client("folder/hello world.txt")
    blob.upload_blob(b"hello from the sdk")
    with pytest.raises(ResourceExistsError):
        blob.upload_blob(b"again")
    blob.upload_blob(b"hello from the sdk, twice", overwrite=True, metadata={"origin": "sdk-tests"})
    assert blob.download_blob().readall() == b"hello from the sdk, twice"
    assert blob.download_blob(offset=6, length=4).readall() == b"from"
    properties = blob.get_blob_properties()
    assert properties.size == 25
    assert properties.metadata == {"origin": "sdk-tests"}

    container.upload_blob("empty", b"")
    assert container.download_blob("empty").readall() == b""
    assert [b.name for b in container.list_blobs()] == ["empty", "folder/hello world.txt"]
    assert sorted(b.name for b in container.walk_blobs(delimiter="/")) == ["empty", "folder/"]

    blob.delete_blob()
    with pytest.raises(ResourceNotFoundError):
        blob.get_blob_properties()
    assert "data" in [c.name for c in service.list_containers()]
    container.delete_container()
    assert "data" not in [c.name for c in service.list_containers()]


@pytest.mark.live
def test_wrong_account_key_is_refused(account):
    with pytest.raises(HttpResponseError) as refused:
        BlobServiceClient(account.blob_url, credential="A" * 88).create_container("nope")
    assert refused.value.status_code == 403


def test_management_plane_under_the_account(storage, credential, group, account):
    assert storage.blob_services.get_service_properties(group, account.name).sku.name == "Standard_LRS"

    # Containers are one store, whichever plane creates them.
    storage.blob_containers.create(group, account.name, "from-arm", {"properties": {"metadata": {"made": "by-arm"}}})
    from_arm = BlobServiceClient(account.blob_url, credential=credential).get_container_client("from-arm")
    assert from_arm.get_container_properties().metadata == {"made": "by-arm"}
    from_arm.upload_blob("x.txt", b"x")
    BlobServiceClient(account.blob_url, credential=credential).create_container("from-data-plane")
    assert sorted(c.name for c in storage.blob_containers.list(group, account.name)) == ["from-arm", "from-data-plane"]
    storage.blob_containers.delete(group, account.name, "from-data-plane")
    assert [c.name for c in storage.blob_containers.list(group, account.name)] == ["from-arm"]

    storage.queue.create(group, account.name, "jobs", {})
    storage.table.create(group, account.name, "Customers")
    share = storage.file_shares.create(group, account.name, "docs", {"properties": {"shareQuota": 10}})
    assert share.share_quota == 10
    assert share.enabled_protocols == "SMB"
    scope = storage.encryption_scopes.put(group, account.name, "tenant-a", {})
    assert scope.state == "Enabled"
    assert scope.source == "Microsoft.Storage"


def test_lifecycle_management_policy(storage, group, account):
    rule = {
        "name": "expire",
        "enabled": True,
        "type": "Lifecycle",
        "definition": {
            "actions": {"baseBlob": {"delete": {"daysAfterModificationGreaterThan": 30}}},
            "filters": {"blobTypes": ["blockBlob"]},
        },
    }
    storage.management_policies.create_or_update(group, account.name, "default", {"properties": {"policy": {"rules": [rule]}}})
    assert storage.management_policies.get(group, account.name, "default").policy.rules[0].name == "expire"


@pytest.fixture
def signed(account):
    """A container `sas` holding `a.txt`, created with an account SAS."""
    token = generate_account_sas(
        account.name,
        account.key,
        ResourceTypes(service=True, container=True, object=True),
        AccountSasPermissions(read=True, write=True, delete=True, list=True, create=True),
        expiry(),
    )
    service = BlobServiceClient(account.blob_url, credential=token)
    service.create_container("sas").upload_blob("a.txt", b"signed")
    assert "sas" in [c.name for c in service.list_containers()]
    return account


def test_sdk_signed_tokens_are_accepted(signed):
    read_only = generate_account_sas(signed.name, signed.key, ResourceTypes(object=True), AccountSasPermissions(read=True), expiry())
    assert BlobClient(signed.blob_url, "sas", "a.txt", credential=read_only).download_blob().readall() == b"signed"

    blob_sas = generate_blob_sas(
        signed.name, "sas", "a.txt", account_key=signed.key, permission=BlobSasPermissions(read=True), expiry=expiry()
    )
    assert BlobClient(signed.blob_url, "sas", "a.txt", credential=blob_sas).download_blob().readall() == b"signed"

    container_sas = generate_container_sas(
        signed.name, "sas", account_key=signed.key, permission=ContainerSasPermissions(read=True, list=True), expiry=expiry()
    )
    assert [b.name for b in ContainerClient(signed.blob_url, "sas", credential=container_sas).list_blobs()] == ["a.txt"]


REFUSALS = {
    "read-only token used to write": lambda a: BlobClient(
        a.blob_url, "sas", "b.txt",
        credential=generate_account_sas(a.name, a.key, ResourceTypes(object=True), AccountSasPermissions(read=True), expiry()),
    ).upload_blob(b"x"),
    "blob token used on another blob": lambda a: BlobClient(
        a.blob_url, "sas", "other.txt",
        credential=generate_blob_sas(a.name, "sas", "a.txt", account_key=a.key, permission=BlobSasPermissions(read=True), expiry=expiry()),
    ).download_blob().readall(),
    "expired token": lambda a: BlobClient(
        a.blob_url, "sas", "a.txt",
        credential=generate_blob_sas(a.name, "sas", "a.txt", account_key=a.key, permission=BlobSasPermissions(read=True), expiry=expiry(-1)),
    ).download_blob().readall(),
    "token signed with the wrong key": lambda a: BlobClient(
        a.blob_url, "sas", "a.txt",
        credential=generate_blob_sas(a.name, "sas", "a.txt", account_key="A" * 88, permission=BlobSasPermissions(read=True), expiry=expiry()),
    ).download_blob().readall(),
}


@pytest.mark.parametrize("case", list(REFUSALS))
def test_bad_tokens_are_refused(signed, case):
    with pytest.raises(HttpResponseError) as refused:
        REFUSALS[case](signed)
    assert refused.value.status_code == 403


def test_token_issued_by_resource_manager(storage, group, signed):
    token = storage.storage_accounts.list_account_sas(
        group,
        signed.name,
        {
            "signedServices": "b",
            "signedResourceTypes": "o",
            "signedPermission": "r",
            "signedExpiryTime": expiry().strftime("%Y-%m-%dT%H:%M:%SZ"),
        },
    ).account_sas_token
    assert BlobClient(signed.blob_url, "sas", "a.txt", credential=token).download_blob().readall() == b"signed"


def test_legal_hold_is_enforced_on_the_data_plane(storage, credential, group, account):
    storage.blob_containers.create(group, account.name, "records", {})
    container = BlobServiceClient(account.blob_url, credential=credential).get_container_client("records")
    container.upload_blob("a.txt", b"v1")

    hold = storage.blob_containers.set_legal_hold(group, account.name, "records", {"tags": ["case123"]})
    assert hold.has_legal_hold and hold.tags == ["case123"]
    assert container.get_container_properties().has_legal_hold
    with pytest.raises(HttpResponseError) as refused:
        container.delete_blob("a.txt")
    assert refused.value.error_code == "BlobImmutableDueToLegalHold"
    with pytest.raises(HttpResponseError):
        container.upload_blob("a.txt", b"v2", overwrite=True)

    cleared = storage.blob_containers.clear_legal_hold(group, account.name, "records", {"tags": ["case123"]})
    assert not cleared.has_legal_hold
    container.delete_blob("a.txt")


def test_immutability_policy_lock_and_extend(storage, credential, group, account):
    storage.blob_containers.create(group, account.name, "retained", {})
    container = BlobServiceClient(account.blob_url, credential=credential).get_container_client("retained")
    container.upload_blob("a.txt", b"kept")

    policy = storage.blob_containers.create_or_update_immutability_policy(
        group, account.name, "retained", parameters={"properties": {"immutabilityPeriodSinceCreationInDays": 3}}
    )
    assert policy.state == "Unlocked"
    with pytest.raises(HttpResponseError) as refused:
        container.delete_blob("a.txt")
    assert refused.value.error_code == "BlobImmutableDueToPolicy"

    locked = storage.blob_containers.lock_immutability_policy(
        group, account.name, "retained", etag=policy.etag, match_condition=MatchConditions.IfNotModified
    )
    assert locked.state == "Locked"
    extended = storage.blob_containers.extend_immutability_policy(
        group, account.name, "retained", etag=locked.etag, match_condition=MatchConditions.IfNotModified,
        parameters={"properties": {"immutabilityPeriodSinceCreationInDays": 10}},
    )
    assert extended.immutability_period_since_creation_in_days == 10
    with pytest.raises(HttpResponseError):
        storage.blob_containers.delete_immutability_policy(
            group, account.name, "retained", etag=extended.etag, match_condition=MatchConditions.IfNotModified
        )
    assert storage.blob_containers.get(group, account.name, "retained").has_immutability_policy


def test_container_lease(storage, credential, group, account):
    storage.blob_containers.create(group, account.name, "leased", {})
    acquired = storage.blob_containers.lease(group, account.name, "leased", {"action": "Acquire", "leaseDuration": -1})
    assert storage.blob_containers.get(group, account.name, "leased").lease_state == "Leased"

    container = BlobServiceClient(account.blob_url, credential=credential).get_container_client("leased")
    with pytest.raises(HttpResponseError) as refused:
        container.delete_container()
    assert refused.value.status_code == 412

    storage.blob_containers.lease(group, account.name, "leased", {"action": "Release", "leaseId": acquired.lease_id})
    container.delete_container()


def test_file_share_lease(storage, group, account):
    storage.file_shares.create(group, account.name, "docs", {})
    acquired = storage.file_shares.lease(group, account.name, "docs", parameters={"action": "Acquire", "leaseDuration": -1})
    assert storage.file_shares.get(group, account.name, "docs").lease_state == "leased"
    with pytest.raises(HttpResponseError) as refused:
        storage.file_shares.delete(group, account.name, "docs")
    assert refused.value.status_code == 412

    storage.file_shares.lease(group, account.name, "docs", parameters={"action": "Release", "leaseId": acquired.lease_id})
    storage.file_shares.delete(group, account.name, "docs")


def test_revoke_user_delegation_keys(storage, group, account):
    storage.storage_accounts.revoke_user_delegation_keys(group, account.name)


def test_object_replication(storage, credential, group):
    from conftest import unique

    accounts = []
    for _ in range(2):
        name = unique("st")
        created = storage.storage_accounts.begin_create(
            group, name, {"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}}
        ).result()
        storage.blob_services.set_service_properties(
            group, name, {"properties": {"isVersioningEnabled": True, "changeFeed": {"enabled": True}}}
        )
        accounts.append((name, created.primary_endpoints.blob))
    (source, source_url), (destination, destination_url) = accounts
    storage.blob_containers.create(group, source, "outbox", {})
    storage.blob_containers.create(group, destination, "inbox", {})

    rules = [{"sourceContainer": "outbox", "destinationContainer": "inbox"}]
    body = {"properties": {"sourceAccount": source, "destinationAccount": destination, "rules": rules}}
    on_destination = storage.object_replication_policies.create_or_update(group, destination, "default", body)
    body["properties"]["rules"][0]["ruleId"] = on_destination.rules[0].rule_id
    on_source = storage.object_replication_policies.create_or_update(group, source, on_destination.policy_id, body)
    assert on_source.enabled_time is not None

    BlobServiceClient(source_url, credential=credential).get_container_client("outbox").upload_blob("a.txt", b"replicated")
    replica = BlobServiceClient(destination_url, credential=credential).get_blob_client("inbox", "a.txt")
    assert replica.download_blob().readall() == b"replicated"


@pytest.mark.live
def test_block_uploads(account):
    # Small limits make the SDK upload in blocks, as it does for large blobs.
    service = BlobServiceClient(account.blob_url, credential=account.key, max_single_put_size=1024, max_block_size=1024)
    container = service.create_container("blocks")
    data = bytes(range(256)) * 20  # 5120 bytes: five blocks
    blob = container.get_blob_client("big.bin")
    blob.upload_blob(data, content_settings=ContentSettings(content_type="application/x-test"), metadata={"kind": "big"})
    assert blob.download_blob().readall() == data
    committed, uncommitted = blob.get_block_list("all")
    assert [b.size for b in committed] == [1024] * 5
    assert uncommitted == []
    properties = blob.get_blob_properties()
    assert properties.size == 5120
    assert properties.content_settings.content_type == "application/x-test"
    assert properties.metadata == {"kind": "big"}
    # In parallel, as the SDK uploads large files.
    parallel = container.get_blob_client("parallel.bin")
    parallel.upload_blob(data * 4, max_concurrency=4)
    assert parallel.download_blob().readall() == data * 4


@pytest.mark.live
def test_staged_blocks(account):
    from azure.storage.blob import BlobBlock, BlockState

    container = BlobServiceClient(account.blob_url, credential=account.key).create_container("staged")
    blob = container.get_blob_client("parts.txt")
    for block_id, text in [("aaaa", b"one,"), ("bbbb", b"two,"), ("cccc", b"three")]:
        blob.stage_block(block_id, text)
    committed, uncommitted = blob.get_block_list("all")
    assert committed == [] and sorted(b.id for b in uncommitted) == ["aaaa", "bbbb", "cccc"]
    # Nothing is visible until the list is committed, and only what it names.
    with pytest.raises(ResourceNotFoundError):
        blob.download_blob()
    blob.commit_block_list([BlobBlock("cccc"), BlobBlock("aaaa")])
    assert blob.download_blob().readall() == b"threeone,"
    assert blob.get_block_list("uncommitted")[1] == []

    # Committed blocks can be reused, reordered and mixed with new ones.
    blob.stage_block("dddd", b"!")
    blob.commit_block_list(
        [BlobBlock("aaaa", BlockState.COMMITTED), BlobBlock("dddd", BlockState.UNCOMMITTED), BlobBlock("cccc", BlockState.COMMITTED)]
    )
    assert blob.download_blob().readall() == b"one,!three"
    with pytest.raises(HttpResponseError) as error:
        blob.commit_block_list([BlobBlock("zzzz")])
    assert error.value.error_code == "InvalidBlockList"
    # Every block id of a blob has the same length.
    with pytest.raises(HttpResponseError) as error:
        blob.stage_block("longer-id", b"x")
    assert error.value.error_code == "InvalidBlobOrBlock"


@pytest.mark.live
def test_blob_metadata_and_headers(account):
    container = BlobServiceClient(account.blob_url, credential=account.key).create_container("props")
    blob = container.get_blob_client("page.html")
    blob.upload_blob(b"<p>hi</p>", content_settings=ContentSettings(content_type="text/html", cache_control="max-age=60"))
    etag = blob.get_blob_properties().etag
    blob.set_blob_metadata({"owner": "tests", "stage": "draft"})
    properties = blob.get_blob_properties()
    assert properties.metadata == {"owner": "tests", "stage": "draft"}
    assert properties.content_settings.cache_control == "max-age=60"
    assert properties.etag != etag
    blob.set_http_headers(ContentSettings(content_type="text/plain", content_language="en", content_disposition="inline"))
    settings = blob.get_blob_properties().content_settings
    assert (settings.content_type, settings.content_language, settings.content_disposition) == ("text/plain", "en", "inline")
    # Headers not given are cleared, as in Azure.
    assert settings.cache_control is None
    assert blob.download_blob().readall() == b"<p>hi</p>"
