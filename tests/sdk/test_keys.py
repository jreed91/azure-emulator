"""The Key Vault keys data plane through the keys SDK. CryptographyClient does
some operations locally with the `cryptography` package, so these tests also
check that the emulator's keys, ciphertexts and signatures interoperate with an
independent implementation."""

import hashlib

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.keyvault.keys import KeyClient, KeyCurveName, KeyOperation, KeyType
from azure.keyvault.keys.crypto import CryptographyClient, EncryptionAlgorithm, KeyWrapAlgorithm, SignatureAlgorithm


@pytest.fixture
def keys(vault, credential):
    return KeyClient(vault.properties.vault_uri, credential)


def test_rsa_key_lifecycle(keys, credential):
    key = keys.create_rsa_key("app-key", size=2048, tags={"use": "tests"})
    assert key.key_type == KeyType.rsa
    assert key.key.n and key.key.e == b"\x01\x00\x01"
    assert keys.get_key("app-key").id == key.id
    assert [k.name for k in keys.list_properties_of_keys()] == ["app-key"]

    crypto = CryptographyClient(key, credential)
    # Encrypted locally with the public key, decrypted by the emulator.
    for algorithm in [EncryptionAlgorithm.rsa_oaep_256, EncryptionAlgorithm.rsa_oaep, EncryptionAlgorithm.rsa1_5]:
        sealed = crypto.encrypt(algorithm, b"plaintext")
        assert crypto.decrypt(algorithm, sealed.ciphertext).plaintext == b"plaintext"
    # Signed by the emulator, verified locally.
    digest = hashlib.sha256(b"document").digest()
    for algorithm in [SignatureAlgorithm.rs256, SignatureAlgorithm.ps256]:
        signature = crypto.sign(algorithm, digest).signature
        assert crypto.verify(algorithm, digest, signature).is_valid
        assert not crypto.verify(algorithm, hashlib.sha256(b"forged").digest(), signature).is_valid
    wrapped = crypto.wrap_key(KeyWrapAlgorithm.rsa_oaep_256, b"\x07" * 32)
    assert crypto.unwrap_key(KeyWrapAlgorithm.rsa_oaep_256, wrapped.encrypted_key).key == b"\x07" * 32

    # A remote-only client sends everything to the emulator.
    remote = CryptographyClient(key.id, credential)
    sealed = remote.encrypt(EncryptionAlgorithm.rsa_oaep_256, b"remote")
    assert remote.decrypt(EncryptionAlgorithm.rsa_oaep_256, sealed.ciphertext).plaintext == b"remote"

    rotated = keys.rotate_key("app-key")
    assert rotated.properties.version != key.properties.version
    assert len(list(keys.list_properties_of_key_versions("app-key"))) == 2
    # The old version still decrypts what it encrypted.
    assert CryptographyClient(key, credential).decrypt(EncryptionAlgorithm.rsa_oaep_256, sealed.ciphertext).plaintext == b"remote"


def test_ec_keys(keys, credential):
    for curve, algorithm, hash_function in [
        (KeyCurveName.p_256, SignatureAlgorithm.es256, hashlib.sha256),
        (KeyCurveName.p_384, SignatureAlgorithm.es384, hashlib.sha384),
        (KeyCurveName.p_521, SignatureAlgorithm.es512, hashlib.sha512),
        (KeyCurveName.p_256_k, SignatureAlgorithm.es256_k, hashlib.sha256),
    ]:
        name = "ec-" + curve.value.lower().replace("-", "")
        key = keys.create_ec_key(name, curve=curve)
        assert key.key.crv == curve
        crypto = CryptographyClient(key, credential)
        digest = hash_function(b"payload").digest()
        signature = crypto.sign(algorithm, digest).signature
        assert crypto.verify(algorithm, digest, signature).is_valid, curve


def test_update_delete_recover_purge(keys, credential):
    key = keys.create_rsa_key("ops-key", key_operations=[KeyOperation.sign, KeyOperation.verify])
    crypto = CryptographyClient(key.id, credential)
    with pytest.raises(HttpResponseError):
        crypto.encrypt(EncryptionAlgorithm.rsa_oaep_256, b"not allowed")

    updated = keys.update_key_properties("ops-key", enabled=False, tags={"state": "off"})
    assert updated.properties.enabled is False and updated.properties.tags == {"state": "off"}
    with pytest.raises(HttpResponseError):
        crypto.sign(SignatureAlgorithm.rs256, hashlib.sha256(b"x").digest())

    deleted = keys.begin_delete_key("ops-key").result()
    assert deleted.recovery_id.endswith("/deletedkeys/ops-key")
    with pytest.raises(ResourceNotFoundError):
        keys.get_key("ops-key")
    assert [k.name for k in keys.list_deleted_keys()] == ["ops-key"]
    keys.begin_recover_deleted_key("ops-key").result()
    assert keys.get_key("ops-key").properties.enabled is False
    keys.begin_delete_key("ops-key").result()
    keys.purge_deleted_key("ops-key")
    assert list(keys.list_deleted_keys()) == []


def test_import_key(keys, credential):
    from azure.keyvault.keys import JsonWebKey
    from cryptography.hazmat.primitives.asymmetric import ec

    private = ec.generate_private_key(ec.SECP256R1())
    numbers = private.private_numbers()
    to_bytes = lambda n: n.to_bytes(32, "big")
    jwk = JsonWebKey(
        kty="EC",
        crv="P-256",
        x=to_bytes(numbers.public_numbers.x),
        y=to_bytes(numbers.public_numbers.y),
        d=to_bytes(numbers.private_value),
        key_ops=["sign", "verify"],
    )
    imported = keys.import_key("imported", jwk)
    assert imported.key.x == to_bytes(numbers.public_numbers.x)
    digest = hashlib.sha256(b"imported").digest()
    signature = CryptographyClient(imported.id, credential).sign(SignatureAlgorithm.es256, digest).signature
    # The key generated here verifies what the emulator signed with its copy.
    from cryptography.hazmat.primitives.asymmetric.utils import Prehashed, encode_dss_signature
    from cryptography.hazmat.primitives import hashes

    der = encode_dss_signature(int.from_bytes(signature[:32], "big"), int.from_bytes(signature[32:], "big"))
    private.public_key().verify(der, digest, ec.ECDSA(Prehashed(hashes.SHA256())))


def test_management_and_data_plane_share_keys(keyvault, keys, group, vault):
    created = keys.create_rsa_key("shared")
    from_arm = keyvault.keys.get(group, vault.name, "shared")
    assert from_arm.key_uri_with_version == created.id
