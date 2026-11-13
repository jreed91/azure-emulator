"""Key Vault keys: the data plane, and the cryptographic operations."""

import hashlib

import pytest
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.keyvault.keys import JsonWebKey, KeyClient, KeyCurveName, KeyRotationLifetimeAction, KeyRotationPolicy, KeyRotationPolicyAction
from azure.keyvault.keys.crypto import CryptographyClient, EncryptionAlgorithm, KeyWrapAlgorithm, SignatureAlgorithm
from cryptography.hazmat.primitives.asymmetric import ec, rsa


def test_key_lifecycle(credential, vault):
    keys = KeyClient(vault.properties.vault_uri, credential)
    created = keys.create_rsa_key("signing", size=3072, tags={"team": "a"})
    assert (created.key_type, len(created.key.n) * 8, created.properties.tags) == ("RSA", 3072, {"team": "a"})
    assert created.id == f"{vault.properties.vault_uri}keys/signing/{created.properties.version}"
    second = keys.create_rsa_key("signing")
    assert keys.get_key("signing").properties.version == second.properties.version
    assert keys.get_key("signing", created.properties.version).key.n == created.key.n

    ec_key = keys.create_ec_key("curve", curve=KeyCurveName.p_384)
    assert (ec_key.key_type, ec_key.key.crv, len(ec_key.key.x)) == ("EC", "P-384", 48)
    assert sorted(k.name for k in keys.list_properties_of_keys()) == ["curve", "signing"]
    assert len(list(keys.list_properties_of_key_versions("signing"))) == 2

    updated = keys.update_key_properties("signing", enabled=False, key_operations=["sign", "verify"], tags={"team": "b"})
    assert (updated.properties.enabled, updated.key_operations, updated.properties.tags) == (False, ["sign", "verify"], {"team": "b"})

    deleted = keys.begin_delete_key("signing").result()
    assert deleted.recovery_id.endswith("/deletedkeys/signing")
    with pytest.raises(ResourceNotFoundError):
        keys.get_key("signing")
    with pytest.raises(ResourceExistsError):
        keys.create_rsa_key("signing")
    assert [k.name for k in keys.list_deleted_keys()] == ["signing"]
    keys.begin_recover_deleted_key("signing").result()
    assert keys.get_key("signing").key.n == second.key.n
    keys.begin_delete_key("signing").wait()
    keys.purge_deleted_key("signing")
    with pytest.raises(ResourceNotFoundError):
        keys.get_deleted_key("signing")


def test_backup_restore_rotate_and_policy(credential, vault):
    keys = KeyClient(vault.properties.vault_uri, credential)
    original = keys.create_ec_key("portable", curve=KeyCurveName.p_256)
    backup = keys.backup_key("portable")
    keys.begin_delete_key("portable").wait()
    keys.purge_deleted_key("portable")
    restored = keys.restore_key_backup(backup)
    assert (restored.name, restored.key.x, restored.properties.version) == ("portable", original.key.x, original.properties.version)
    with pytest.raises(ResourceExistsError):
        keys.restore_key_backup(backup)

    rotated = keys.rotate_key("portable")
    assert rotated.properties.version != original.properties.version and rotated.key.x != original.key.x
    assert keys.get_key_rotation_policy("portable").lifetime_actions[0].action == KeyRotationPolicyAction.notify
    policy = keys.update_key_rotation_policy(
        "portable",
        KeyRotationPolicy(
            lifetime_actions=[KeyRotationLifetimeAction(KeyRotationPolicyAction.rotate, time_after_create="P90D")],
            expires_in="P1Y",
        ),
    )
    assert (policy.expires_in, policy.lifetime_actions[0].time_after_create) == ("P1Y", "P90D")
    assert keys.get_key_rotation_policy("portable").expires_in == "P1Y"


def test_import(credential, vault):
    keys = KeyClient(vault.properties.vault_uri, credential)
    private = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    numbers = private.private_numbers()
    to_bytes = lambda n: n.to_bytes((n.bit_length() + 7) // 8, "big")
    jwk = JsonWebKey(
        kty="RSA", n=to_bytes(numbers.public_numbers.n), e=to_bytes(numbers.public_numbers.e), d=to_bytes(numbers.d),
        p=to_bytes(numbers.p), q=to_bytes(numbers.q), dp=to_bytes(numbers.dmp1), dq=to_bytes(numbers.dmq1), qi=to_bytes(numbers.iqmp),
    )
    imported = keys.import_key("brought", jwk)
    assert imported.key.n == to_bytes(numbers.public_numbers.n)
    # The vault decrypts what the local private key's public half encrypted.
    from cryptography.hazmat.primitives.asymmetric import padding
    from cryptography.hazmat.primitives import hashes
    sealed = private.public_key().encrypt(b"hello", padding.OAEP(mgf=padding.MGF1(hashes.SHA256()), algorithm=hashes.SHA256(), label=None))
    crypto = CryptographyClient(imported, credential)
    assert crypto.decrypt(EncryptionAlgorithm.rsa_oaep_256, sealed).plaintext == b"hello"

    with pytest.raises(HttpResponseError):
        keys.import_key("public-only", JsonWebKey(kty="RSA", n=jwk.n, e=jwk.e))


@pytest.mark.parametrize("algorithm", [EncryptionAlgorithm.rsa_oaep, EncryptionAlgorithm.rsa_oaep_256, EncryptionAlgorithm.rsa1_5])
def test_rsa_encryption(credential, vault, algorithm):
    key = KeyClient(vault.properties.vault_uri, credential).create_rsa_key("sealer")
    crypto = CryptographyClient(key.id, credential)
    sealed = crypto.encrypt(algorithm, b"plain text")
    assert crypto.decrypt(algorithm, sealed.ciphertext).plaintext == b"plain text"
    wrapped = crypto.wrap_key(KeyWrapAlgorithm(algorithm.value), b"k" * 32)
    assert crypto.unwrap_key(KeyWrapAlgorithm(algorithm.value), wrapped.encrypted_key).key == b"k" * 32


@pytest.mark.parametrize(
    ("kind", "algorithm", "hash"),
    [
        ("rsa", SignatureAlgorithm.rs256, hashlib.sha256),
        ("rsa", SignatureAlgorithm.ps384, hashlib.sha384),
        (KeyCurveName.p_256, SignatureAlgorithm.es256, hashlib.sha256),
        (KeyCurveName.p_256_k, SignatureAlgorithm.es256_k, hashlib.sha256),
        (KeyCurveName.p_384, SignatureAlgorithm.es384, hashlib.sha384),
        (KeyCurveName.p_521, SignatureAlgorithm.es512, hashlib.sha512),
    ],
)
def test_signatures(credential, vault, kind, algorithm, hash):
    keys = KeyClient(vault.properties.vault_uri, credential)
    key = keys.create_rsa_key("signer") if kind == "rsa" else keys.create_ec_key("signer", curve=kind)
    # The client verifies locally with the public key, so this also checks the
    # key's JSON Web Key and the signature's encoding.
    crypto = CryptographyClient(key, credential)
    digest = hash(b"message").digest()
    signature = crypto.sign(algorithm, digest).signature
    assert crypto.verify(algorithm, digest, signature).is_valid
    assert not crypto.verify(algorithm, hash(b"other").digest(), signature).is_valid
    # And remotely.
    remote = CryptographyClient(key.id, credential)
    assert remote.verify(algorithm, digest, signature).is_valid


def test_operations_are_checked(credential, vault):
    keys = KeyClient(vault.properties.vault_uri, credential)
    key = keys.create_rsa_key("limited", key_operations=["encrypt", "decrypt"])
    crypto = CryptographyClient(key.id, credential)
    with pytest.raises(HttpResponseError) as refused:
        crypto.sign(SignatureAlgorithm.rs256, hashlib.sha256(b"m").digest())
    assert refused.value.status_code == 403
    keys.update_key_properties("limited", enabled=False)
    with pytest.raises(HttpResponseError) as refused:
        crypto.decrypt(EncryptionAlgorithm.rsa_oaep, b"x" * 256)
    assert refused.value.status_code == 403


def test_keys_on_resource_manager_and_data_plane(keyvault, group, credential, vault):
    keyvault.keys.create_if_not_exist(group, vault.name, "from-arm", {"properties": {"kty": "EC", "curveName": "P-256"}})
    keys = KeyClient(vault.properties.vault_uri, credential)
    key = keys.get_key("from-arm")
    crypto = CryptographyClient(key, credential)
    digest = hashlib.sha256(b"m").digest()
    assert crypto.verify(SignatureAlgorithm.es256, digest, crypto.sign(SignatureAlgorithm.es256, digest).signature).is_valid
    keys.create_rsa_key("from-data-plane")
    assert sorted(k.name for k in keyvault.keys.list(group, vault.name)) == ["from-arm", "from-data-plane"]
