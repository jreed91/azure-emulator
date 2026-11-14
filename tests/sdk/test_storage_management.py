"""The Storage control plane under an account: service properties, containers,
queues, tables and shares through Resource Manager (and their agreement with
the data planes), encryption scopes, local users, policies, private link
resources, and the refusals of the container protections.

Expected values follow the Storage resource provider REST reference."""

import pytest
from azure.core import MatchConditions
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.data.tables import TableServiceClient
from azure.storage.blob import BlobServiceClient
from azure.storage.fileshare import ShareServiceClient
from azure.storage.queue import QueueServiceClient


def status_of(call):
    with pytest.raises(HttpResponseError) as refused:
        call()
    return refused.value.status_code


def test_service_properties(storage, group, account):
    blob = storage.blob_services.get_service_properties(group, account.name)
    assert blob.name == "default" and blob.sku.name == "Standard_LRS"
    assert blob.delete_retention_policy.enabled is False

    updated = storage.blob_services.set_service_properties(
        group,
        account.name,
        {"properties": {"isVersioningEnabled": True, "deleteRetentionPolicy": {"enabled": True, "days": 7}}},
    )
    assert updated.is_versioning_enabled is True and updated.delete_retention_policy.days == 7
    # An update keeps what it does not mention.
    again = storage.blob_services.set_service_properties(group, account.name, {"properties": {"changeFeed": {"enabled": True}}})
    assert again.is_versioning_enabled is True and again.change_feed.enabled is True

    files = storage.file_services.get_service_properties(group, account.name)
    assert (files.share_delete_retention_policy.enabled, files.share_delete_retention_policy.days) == (True, 7)
    cors = {"allowedOrigins": ["https://example.com"], "allowedMethods": ["GET"], "allowedHeaders": ["*"], "exposedHeaders": ["*"], "maxAgeInSeconds": 60}
    queues = storage.queue_services.set_service_properties(group, account.name, {"properties": {"cors": {"corsRules": [cors]}}})
    assert queues.cors.cors_rules[0].allowed_origins == ["https://example.com"]
    tables = storage.table_services.get_service_properties(group, account.name)
    assert tables.cors.cors_rules == []

    assert [s.name for s in storage.blob_services.list(group, account.name)] == ["default"]
    assert [s.name for s in storage.file_services.list(group, account.name).value] == ["default"]


def test_containers_through_resource_manager(storage, group, account):
    containers = storage.blob_containers
    created = containers.create(group, account.name, "docs", {"properties": {"publicAccess": "Blob", "metadata": {"owner": "team"}}})
    assert (created.public_access, created.metadata, created.lease_state) == ("Blob", {"owner": "team"}, "Available")
    assert created.has_immutability_policy is False and created.has_legal_hold is False

    # The same container on the Blob data plane.
    data = BlobServiceClient(account.blob_url, credential=account.key)
    assert data.get_container_client("docs").get_container_properties().metadata == {"owner": "team"}
    data.create_container("made-on-data-plane")
    assert {"docs", "made-on-data-plane"} <= {c.name for c in containers.list(group, account.name)}

    updated = containers.update(group, account.name, "docs", {"properties": {"publicAccess": "None"}})
    assert updated.public_access == "None" and updated.metadata == {"owner": "team"}
    assert updated.etag != created.etag

    assert status_of(lambda: containers.update(group, account.name, "missing", {"properties": {}})) == 404
    assert status_of(lambda: containers.create(group, account.name, "Bad_Name", {})) == 400
    assert status_of(lambda: containers.create(group, account.name, "docs", {"properties": {"publicAccess": "Everyone"}})) == 400
    assert containers.get(group, account.name, "docs").public_access == "None", "a refused update changes nothing"
    with pytest.raises(ResourceNotFoundError):
        containers.get(group, account.name, "missing")

    containers.delete(group, account.name, "docs")
    assert "docs" not in {c.name for c in containers.list(group, account.name)}
    containers.delete(group, account.name, "docs")  # already gone: not an error


def test_queues_tables_and_shares_through_resource_manager(storage, group, account):
    queue = storage.queue.create(group, account.name, "jobs", {"properties": {"metadata": {"kind": "work"}}})
    assert queue.metadata == {"kind": "work"} and queue.approximate_message_count == 0
    data_queue = QueueServiceClient(account.queue_url, credential=account.key).get_queue_client("jobs")
    assert data_queue.get_queue_properties().metadata == {"kind": "work"}
    data_queue.send_message("hello")
    assert storage.queue.get(group, account.name, "jobs").approximate_message_count == 1
    assert status_of(lambda: storage.queue.create(group, account.name, "Bad_Queue", {})) == 400
    storage.queue.delete(group, account.name, "jobs")
    assert "jobs" not in {q.name for q in QueueServiceClient(account.queue_url, credential=account.key).list_queues()}

    table = storage.table.create(group, account.name, "Orders")
    assert table.table_name == "Orders"
    data_tables = TableServiceClient(endpoint=account.table_url, credential=_named_key(account))
    assert "Orders" in {t.name for t in data_tables.list_tables()}
    for bad in ("1orders", "or", "has-hyphen"):
        assert status_of(lambda: storage.table.create(group, account.name, bad)) == 400, bad
    storage.table.delete(group, account.name, "Orders")
    assert "Orders" not in {t.name for t in data_tables.list_tables()}

    share = storage.file_shares.create(group, account.name, "docs", {"properties": {"shareQuota": 10}})
    assert (share.share_quota, share.enabled_protocols, share.access_tier) == (10, "SMB", "TransactionOptimized")
    data_share = ShareServiceClient(account.file_url, credential=account.key).get_share_client("docs")
    assert data_share.get_share_properties().quota == 10
    resized = storage.file_shares.update(group, account.name, "docs", {"properties": {"shareQuota": 20, "accessTier": "Cool"}})
    assert (resized.share_quota, resized.access_tier) == (20, "Cool")
    assert data_share.get_share_properties().quota == 20
    for quota in (0, 102401):
        assert status_of(lambda: storage.file_shares.create(group, account.name, "other", {"properties": {"shareQuota": quota}})) == 400
    assert status_of(lambda: storage.file_shares.update(group, account.name, "docs", {"properties": {"enabledProtocols": "NFS"}})) == 400
    assert status_of(lambda: storage.file_shares.create(group, account.name, "Bad_Share", {})) == 400
    assert [s.name for s in storage.file_shares.list(group, account.name)] == ["docs"]


def _named_key(account):
    from azure.core.credentials import AzureNamedKeyCredential

    return AzureNamedKeyCredential(account.name, account.key)


def test_share_lease_blocks_deletion(storage, group, account):
    storage.file_shares.create(group, account.name, "leased", {})
    lease = storage.file_shares.lease(group, account.name, "leased", parameters={"action": "Acquire", "leaseDuration": 30})
    share = storage.file_shares.get(group, account.name, "leased")
    assert (share.lease_state, share.lease_status, share.lease_duration) == ("leased", "locked", "fixed")
    assert status_of(lambda: storage.file_shares.delete(group, account.name, "leased")) == 412
    storage.file_shares.lease(group, account.name, "leased", parameters={"action": "Break", "breakPeriod": 0})
    storage.file_shares.delete(group, account.name, "leased")
    assert lease.lease_id


@pytest.mark.parametrize(
    "body, ok",
    [
        ({}, True),
        ({"properties": {"source": "Microsoft.Storage", "state": "Disabled"}}, True),
        ({"properties": {"source": "Microsoft.KeyVault", "keyVaultProperties": {"keyUri": "https://v.vault.azure.net/keys/k"}}}, True),
        ({"properties": {"source": "Microsoft.KeyVault"}}, False),  # Key Vault keys need a key URI
        ({"properties": {"source": "Elsewhere"}}, False),
        ({"properties": {"state": "Paused"}}, False),
    ],
)
def test_encryption_scopes(storage, group, account, body, ok):
    if ok:
        scope = storage.encryption_scopes.put(group, account.name, "scope1", body)
        assert scope.source == body.get("properties", {}).get("source", "Microsoft.Storage")
        assert scope.state == body.get("properties", {}).get("state", "Enabled")
    else:
        assert status_of(lambda: storage.encryption_scopes.put(group, account.name, "scope1", body)) == 400


def test_encryption_scope_updates(storage, group, account):
    scope = storage.encryption_scopes.put(group, account.name, "scope1", {"properties": {"requireInfrastructureEncryption": True}})
    assert scope.require_infrastructure_encryption is True
    disabled = storage.encryption_scopes.patch(group, account.name, "scope1", {"properties": {"state": "Disabled", "requireInfrastructureEncryption": False}})
    assert disabled.state == "Disabled"
    assert disabled.require_infrastructure_encryption is True, "infrastructure encryption is chosen at creation"
    assert [s.name for s in storage.encryption_scopes.list(group, account.name)] == ["scope1"]


def test_local_users(storage, group, account):
    users = storage.local_users
    user = users.create_or_update(
        group,
        account.name,
        "sftpuser",
        {"properties": {"homeDirectory": "home", "hasSharedKey": True, "sshAuthorizedKeys": [{"description": "laptop", "key": "ssh-rsa AAAA"}]}},
    )
    assert user.has_ssh_key is True and user.has_shared_key is True and user.has_ssh_password is False
    assert user.sid.startswith("S-1-2-0-") and user.ssh_authorized_keys is None, "keys are only returned by listKeys"

    keys = users.list_keys(group, account.name, "sftpuser")
    assert [k.key for k in keys.ssh_authorized_keys] == ["ssh-rsa AAAA"]
    assert keys.shared_key and users.list_keys(group, account.name, "sftpuser").shared_key == keys.shared_key

    password = users.regenerate_password(group, account.name, "sftpuser").ssh_password
    assert len(password) == 32
    assert users.regenerate_password(group, account.name, "sftpuser").ssh_password != password
    assert users.get(group, account.name, "sftpuser").has_ssh_password is True

    second = users.create_or_update(group, account.name, "other", {"properties": {"homeDirectory": "x"}})
    assert second.user_id != user.user_id and second.sid != user.sid
    assert "sharedKey" not in (users.list_keys(group, account.name, "other").as_dict())
    assert sorted(u.name for u in users.list(group, account.name)) == ["other", "sftpuser"]

    for bad in ("UPPER", "ab", "has-hyphen"):
        assert status_of(lambda: users.create_or_update(group, account.name, bad, {"properties": {}})) == 400, bad
    users.delete(group, account.name, "other")
    with pytest.raises(ResourceNotFoundError):
        users.get(group, account.name, "other")


def test_policies_refusals(storage, group, account):
    rule = {"name": "r", "enabled": True, "type": "Lifecycle", "definition": {"filters": {"blobTypes": ["blockBlob"]}, "actions": {"baseBlob": {"delete": {"daysAfterModificationGreaterThan": 30}}}}}
    assert status_of(lambda: storage.management_policies.create_or_update(group, account.name, "default", {"properties": {}})) == 400
    assert status_of(lambda: storage.management_policies.create_or_update(group, account.name, "default", {"properties": {"policy": {}}})) == 400
    storage.management_policies.create_or_update(group, account.name, "default", {"properties": {"policy": {"rules": [rule]}}})
    storage.management_policies.delete(group, account.name, "default")
    with pytest.raises(ResourceNotFoundError):
        storage.management_policies.get(group, account.name, "default")

    inventory = {"enabled": True, "rules": [{"name": "daily", "enabled": True, "destination": "inventory", "definition": {"format": "Csv", "schedule": "Daily", "objectType": "Blob", "schemaFields": ["Name"], "filters": {"blobTypes": ["blockBlob"]}}}]}
    created = storage.blob_inventory_policies.create_or_update(group, account.name, "default", {"properties": {"policy": inventory}})
    assert created.policy.rules[0].name == "daily"
    assert [p.name for p in storage.blob_inventory_policies.list(group, account.name)] == ["default"]


def test_private_link_resources_and_migration(storage, group, account):
    groups = [r.group_id for r in storage.private_link_resources.list_by_storage_account(group, account.name).value]
    assert groups == ["blob", "table", "queue", "file", "web", "dfs"], "a locally redundant account has no secondaries"

    geo = account.name[:-2] + "rg"
    storage.storage_accounts.begin_create(group, geo, {"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_RAGRS"}}).result()
    geo_groups = [r.group_id for r in storage.private_link_resources.list_by_storage_account(group, geo).value]
    assert "blob_secondary" in geo_groups and len(geo_groups) == 12
    blob = next(r for r in storage.private_link_resources.list_by_storage_account(group, geo).value if r.group_id == "blob")
    assert blob.required_zone_names == ["privatelink.blob.core.windows.net"]

    migration = storage.storage_accounts.get_customer_initiated_migration(group, account.name, "default")
    assert migration.target_sku_name == "Standard_LRS"
    assert status_of(lambda: storage.private_link_resources.list_by_storage_account(group, "nosuchaccount1")) == 404


def test_immutability_policy_refusals(storage, group, account):
    containers = storage.blob_containers
    containers.create(group, account.name, "kept", {})
    assert status_of(lambda: containers.get_immutability_policy(group, account.name, "kept")) == 404
    for days in (0, 146001):
        assert status_of(lambda: containers.create_or_update_immutability_policy(
            group, account.name, "kept", parameters={"properties": {"immutabilityPeriodSinceCreationInDays": days}}
        )) == 400

    policy = containers.create_or_update_immutability_policy(
        group, account.name, "kept", parameters={"properties": {"immutabilityPeriodSinceCreationInDays": 2, "allowProtectedAppendWrites": True}}
    )
    assert (policy.state, policy.allow_protected_append_writes) == ("Unlocked", True)
    # Extending is for locked policies; locking and deleting need the etag.
    assert status_of(lambda: containers.extend_immutability_policy(
        group, account.name, "kept", etag=policy.etag, match_condition=MatchConditions.IfNotModified, parameters={"properties": {"immutabilityPeriodSinceCreationInDays": 5}}
    )) == 409
    assert status_of(lambda: containers.lock_immutability_policy(group, account.name, "kept", etag='"stale"', match_condition=MatchConditions.IfNotModified)) == 412
    # An unlocked policy can be deleted.
    containers.delete_immutability_policy(group, account.name, "kept", etag=policy.etag, match_condition=MatchConditions.IfNotModified)
    assert containers.get(group, account.name, "kept").has_immutability_policy is False

    policy = containers.create_or_update_immutability_policy(
        group, account.name, "kept", parameters={"properties": {"immutabilityPeriodSinceCreationInDays": 4}}
    )
    locked = containers.lock_immutability_policy(group, account.name, "kept", etag=policy.etag, match_condition=MatchConditions.IfNotModified)
    assert locked.state == "Locked"
    assert status_of(lambda: containers.extend_immutability_policy(
        group, account.name, "kept", etag=locked.etag, match_condition=MatchConditions.IfNotModified, parameters={"properties": {"immutabilityPeriodSinceCreationInDays": 3}}
    )) == 400
    assert status_of(lambda: containers.create_or_update_immutability_policy(
        group, account.name, "kept", parameters={"properties": {"immutabilityPeriodSinceCreationInDays": 9}}
    )) == 409
    assert status_of(lambda: containers.delete_immutability_policy(group, account.name, "kept", etag=locked.etag, match_condition=MatchConditions.IfNotModified)) == 409

    history = containers.get(group, account.name, "kept").immutability_policy.update_history
    assert [h["update"] for h in history][-2:] == ["put", "lock"]


@pytest.mark.parametrize(
    "tags, ok",
    [
        (["abc"], True),
        (["a" * 23], True),
        (["ab"], False),  # 3 to 23 characters
        (["a" * 24], False),
        (["has-hyphen"], False),
        ([], False),
        ([f"tag{i}" for i in range(11)], False),  # at most 10
    ],
)
def test_legal_hold_tags(storage, group, account, tags, ok):
    storage.blob_containers.create(group, account.name, "held", {})
    if ok:
        hold = storage.blob_containers.set_legal_hold(group, account.name, "held", {"tags": tags})
        assert hold.has_legal_hold
        assert status_of(lambda: storage.blob_containers.delete(group, account.name, "held")) == 409
        assert not storage.blob_containers.clear_legal_hold(group, account.name, "held", {"tags": tags}).has_legal_hold
    else:
        assert status_of(lambda: storage.blob_containers.set_legal_hold(group, account.name, "held", {"tags": tags})) == 400


@pytest.mark.parametrize(
    "body, status",
    [
        ({"action": "Acquire", "leaseDuration": 14}, 400),  # 15 to 60, or -1
        ({"action": "Acquire", "leaseDuration": 61}, 400),
        ({"action": "Renew"}, 400),  # needs the lease id
        ({"action": "Release", "leaseId": "00000000-0000-0000-0000-000000000000"}, 409),
        ({"action": "Steal"}, 400),
        ({"action": "Break", "breakPeriod": 61}, 400),
    ],
)
def test_container_lease_refusals(storage, group, account, body, status):
    storage.blob_containers.create(group, account.name, "leased", {})
    storage.blob_containers.lease(group, account.name, "leased", {"action": "Acquire", "leaseDuration": -1})
    assert status_of(lambda: storage.blob_containers.lease(group, account.name, "leased", body)) == status


def test_container_lease_change_and_break(storage, group, account):
    containers = storage.blob_containers
    containers.create(group, account.name, "leased", {})
    first = containers.lease(group, account.name, "leased", {"action": "Acquire", "leaseDuration": 30})
    assert containers.get(group, account.name, "leased").lease_duration == "Fixed"
    assert status_of(lambda: containers.lease(group, account.name, "leased", {"action": "Acquire", "leaseDuration": 30})) == 409
    changed = containers.lease(
        group, account.name, "leased", {"action": "Change", "leaseId": first.lease_id, "proposedLeaseId": "11111111-1111-1111-1111-111111111111"}
    )
    assert changed.lease_id == "11111111-1111-1111-1111-111111111111"
    renewed = containers.lease(group, account.name, "leased", {"action": "Renew", "leaseId": changed.lease_id})
    assert renewed.lease_id == changed.lease_id
    broken = containers.lease(group, account.name, "leased", {"action": "Break", "breakPeriod": 10})
    assert broken.lease_time_seconds == "10"
    assert containers.get(group, account.name, "leased").lease_state == "Breaking"
    containers.lease(group, account.name, "leased", {"action": "Break", "breakPeriod": 0})
    assert containers.get(group, account.name, "leased").lease_state == "Broken"
    containers.delete(group, account.name, "leased")
