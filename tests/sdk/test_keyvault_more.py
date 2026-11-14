"""Key Vault's data plane beyond the happy paths: the server-side crypto
operations (the SDK does some locally, so these go over REST), imports on
every curve, random bytes, and the refusals for keys, secrets and
certificates."""

import base64
import hashlib

import pytest
import requests
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.keyvault.certificates import CertificateClient, CertificatePolicy, KeyCurveName as CertCurve, KeyType as CertKeyType, WellKnownIssuerNames
from azure.keyvault.keys import JsonWebKey, KeyClient, KeyCurveName
from azure.keyvault.keys.crypto import CryptographyClient, EncryptionAlgorithm, SignatureAlgorithm
from azure.keyvault.secrets import SecretClient
from cryptography import x509
from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import ec, padding


def b64url(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def unb64url(text):
    return base64.urlsafe_b64decode(text + "=" * (-len(text) % 4))


class Rest:
    """Signed REST calls to a vault, for operations the SDK does locally."""

    def __init__(self, vault, credential):
        self.base = vault.properties.vault_uri
        self.token = credential.get_token("https://vault.azure.net/.default").token

    def post(self, path, body):
        return requests.post(f"{self.base}{path}?api-version=7.5", json=body, headers={"Authorization": f"Bearer {self.token}"})

    def get(self, path, version="7.5"):
        query = f"?api-version={version}" if version else ""
        return requests.get(f"{self.base}{path}{query}", headers={"Authorization": f"Bearer {self.token}"})


@pytest.mark.parametrize("algorithm", ["RSA1_5", "RSA-OAEP", "RSA-OAEP-256"])
def test_server_side_encrypt_and_wrap(credential, vault, algorithm):
    key = KeyClient(vault.properties.vault_uri, credential).create_rsa_key("sealer")
    rest = Rest(vault, credential)
    path = f"keys/sealer/{key.properties.version}"
    sealed = rest.post(f"{path}/encrypt", {"alg": algorithm, "value": b64url(b"plain text")})
    assert sealed.status_code == 200, sealed.text
    assert sealed.json()["kid"] == key.id
    crypto = CryptographyClient(key.id, credential)
    assert crypto.decrypt(EncryptionAlgorithm(algorithm), unb64url(sealed.json()["value"])).plaintext == b"plain text"
    wrapped = rest.post(f"{path}/wrapkey", {"alg": algorithm, "value": b64url(b"k" * 32)})
    assert wrapped.status_code == 200, wrapped.text
    unwrapped = rest.post(f"{path}/unwrapkey", {"alg": algorithm, "value": wrapped.json()["value"]})
    assert unb64url(unwrapped.json()["value"]) == b"k" * 32


@pytest.mark.parametrize(("algorithm", "hash"), [("RS256", hashlib.sha256), ("PS512", hashlib.sha512)])
def test_server_side_verify(credential, vault, algorithm, hash):
    key = KeyClient(vault.properties.vault_uri, credential).create_rsa_key("verifier")
    digest = hash(b"message").digest()
    signature = CryptographyClient(key.id, credential).sign(SignatureAlgorithm(algorithm), digest).signature
    rest = Rest(vault, credential)
    ok = rest.post(f"keys/verifier/{key.properties.version}/verify", {"alg": algorithm, "digest": b64url(digest), "value": b64url(signature)})
    assert ok.json() == {"value": True}
    bad = rest.post(f"keys/verifier/{key.properties.version}/verify", {"alg": algorithm, "digest": b64url(hash(b"other").digest()), "value": b64url(signature)})
    assert bad.json() == {"value": False}
    # A digest of the wrong length for the algorithm is refused.
    wrong = rest.post(f"keys/verifier/{key.properties.version}/sign", {"alg": algorithm, "value": b64url(b"short")})
    assert wrong.status_code == 400


@pytest.mark.parametrize(
    ("curve", "local", "algorithm", "hash"),
    [
        ("P-384", ec.SECP384R1(), SignatureAlgorithm.es384, hashes.SHA384()),
        ("P-521", ec.SECP521R1(), SignatureAlgorithm.es512, hashes.SHA512()),
        ("P-256K", ec.SECP256K1(), SignatureAlgorithm.es256_k, hashes.SHA256()),
    ],
)
def test_import_on_every_curve(credential, vault, curve, local, algorithm, hash):
    private = ec.generate_private_key(local)
    numbers = private.private_numbers()
    size = (local.key_size + 7) // 8
    jwk = JsonWebKey(
        kty="EC",
        crv=curve,
        d=numbers.private_value.to_bytes(size, "big"),
        x=numbers.public_numbers.x.to_bytes(size, "big"),
        y=numbers.public_numbers.y.to_bytes(size, "big"),
    )
    imported = KeyClient(vault.properties.vault_uri, credential).import_key(f"imported-{curve.lower().replace('-', '')}", jwk)
    assert (imported.key.crv, imported.key.x) == (curve, jwk.x)
    digest_hash = hashes.Hash(hash)
    digest_hash.update(b"payload")
    digest = digest_hash.finalize()
    signature = CryptographyClient(imported.id, credential).sign(algorithm, digest).signature
    # The local key verifies what the vault signed (raw r||s, as JWS has it).
    half = len(signature) // 2
    from cryptography.hazmat.primitives.asymmetric.utils import Prehashed, encode_dss_signature

    der = encode_dss_signature(int.from_bytes(signature[:half], "big"), int.from_bytes(signature[half:], "big"))
    private.public_key().verify(der, digest, ec.ECDSA(Prehashed(hash)))


def test_key_refusals(credential, vault):
    keys = KeyClient(vault.properties.vault_uri, credential)
    with pytest.raises(HttpResponseError):
        keys.create_rsa_key("tiny", size=1024)
    with pytest.raises(HttpResponseError) as refused:
        keys.create_rsa_key("hsm", hardware_protected=True)
    assert "Premium" in str(refused.value)
    with pytest.raises(HttpResponseError):
        keys.create_key("sym", "oct")
    with pytest.raises(HttpResponseError):
        keys.create_ec_key("curve", curve="P-999")
    with pytest.raises(HttpResponseError):
        keys.create_ec_key("ec-encrypt", key_operations=["encrypt"])
    keys.create_ec_key("ec")
    with pytest.raises(HttpResponseError):
        keys.update_key_properties("ec", key_operations=["wrapKey"])
    with pytest.raises(HttpResponseError):
        keys.import_key("symmetric", JsonWebKey(kty="oct", k=b"0" * 32))
    with pytest.raises(ResourceNotFoundError):
        keys.get_key("ec", "0" * 32)
    with pytest.raises(ResourceNotFoundError):
        keys.get_deleted_key("never-deleted")
    keys.begin_delete_key("ec").wait()
    with pytest.raises(ResourceExistsError):
        keys.create_ec_key("ec")
    with pytest.raises(ResourceNotFoundError):
        keys.purge_deleted_key("never-deleted")


def test_random_bytes(credential, vault):
    keys = KeyClient(vault.properties.vault_uri, credential)
    first, second = keys.get_random_bytes(32), keys.get_random_bytes(32)
    assert len(first) == 32 and first != second
    # (The SDK refuses a count of 0 itself.)
    with pytest.raises(HttpResponseError):
        keys.get_random_bytes(200)


def test_secret_refusals_and_deleted_listing(credential, vault):
    secrets = SecretClient(vault.properties.vault_uri, credential)
    first = secrets.set_secret("token", "v1")
    secrets.set_secret("token", "v2")
    secrets.update_secret_properties("token", first.properties.version, enabled=False, content_type="text/old")
    old = [p for p in secrets.list_properties_of_secret_versions("token") if p.version == first.properties.version][0]
    assert (old.enabled, old.content_type) == (False, "text/old")
    secrets.begin_delete_secret("token").wait()
    assert [s.name for s in secrets.list_deleted_secrets()] == ["token"]
    with pytest.raises(ResourceExistsError):
        secrets.set_secret("token", "again")
    with pytest.raises(ResourceNotFoundError):
        secrets.get_secret("token")

    rest = Rest(vault, credential)
    assert rest.get("secrets", version=None).status_code == 400, "api-version is required"
    assert rest.get("secrets/bad_name").status_code == 400
    assert rest.get("nothing-here").status_code == 501


def test_certificate_policies_and_refusals(credential, vault):
    certificates = CertificateClient(vault.properties.vault_uri, credential)
    policy = CertificatePolicy(
        issuer_name=WellKnownIssuerNames.self,
        subject="CN=full.example.test, O=Emulator, OU=Testing, C=NO, L=Oslo, ST=Oslo",
        key_usage=["digitalSignature", "keyEncipherment", "dataEncipherment", "keyAgreement", "keyCertSign", "cRLSign"],
        enhanced_key_usage=["1.3.6.1.5.5.7.3.1", "1.3.6.1.5.5.7.3.3", "1.2.3.4"],
        san_emails=["admin@example.test"],
    )
    created = certificates.begin_create_certificate("full", policy).result()
    cert = x509.load_der_x509_certificate(bytes(created.cer))
    subject = {a.oid._name: a.value for a in cert.subject}
    assert subject == {"commonName": "full.example.test", "organizationName": "Emulator", "organizationalUnitName": "Testing", "countryName": "NO", "localityName": "Oslo", "stateOrProvinceName": "Oslo"}
    usage = cert.extensions.get_extension_for_class(x509.KeyUsage).value
    assert usage.digital_signature and usage.key_encipherment and usage.data_encipherment and usage.key_agreement and usage.key_cert_sign and usage.crl_sign
    ekus = [o.dotted_string for o in cert.extensions.get_extension_for_class(x509.ExtendedKeyUsage).value]
    assert ekus == ["1.3.6.1.5.5.7.3.1", "1.3.6.1.5.5.7.3.3", "1.2.3.4"]
    assert cert.extensions.get_extension_for_class(x509.SubjectAlternativeName).value.get_values_for_type(x509.RFC822Name) == ["admin@example.test"]

    p256 = certificates.begin_create_certificate(
        "ec256", CertificatePolicy(issuer_name=WellKnownIssuerNames.self, subject="CN=ec", key_type=CertKeyType.ec, key_curve_name=CertCurve.p_256)
    ).result()
    ec_cert = x509.load_der_x509_certificate(bytes(p256.cer))
    ec_cert.public_key().verify(ec_cert.signature, ec_cert.tbs_certificate_bytes, ec.ECDSA(ec_cert.signature_hash_algorithm))

    refused = [
        CertificatePolicy(issuer_name=WellKnownIssuerNames.self, subject="CN=x", key_size=1024),
        CertificatePolicy(issuer_name=WellKnownIssuerNames.self, subject="CN=x", key_type=CertKeyType.ec, key_curve_name=CertCurve.p_521),
        CertificatePolicy(issuer_name="DigiCert", subject="CN=x"),
        CertificatePolicy(issuer_name=WellKnownIssuerNames.self, subject="XX=unknown"),
        CertificatePolicy(issuer_name=WellKnownIssuerNames.self, subject="CN=x", validity_in_months=0),
    ]
    for i, bad in enumerate(refused):
        with pytest.raises(HttpResponseError):
            certificates.begin_create_certificate(f"bad{i}", bad).result()

    with pytest.raises(ResourceNotFoundError):
        certificates.get_certificate_operation("no-such")
    certificates.begin_delete_certificate("ec256").wait()
    with pytest.raises(ResourceExistsError):
        certificates.begin_create_certificate("ec256", CertificatePolicy.get_default()).result()
    with pytest.raises(HttpResponseError):
        certificates.import_certificate("no-key", b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n")
