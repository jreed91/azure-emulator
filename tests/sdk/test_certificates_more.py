"""Key Vault certificate imports across key types and encodings: EC keys on
P-384, P-521 and secp256k1 and RSA keys, as PKCS#8, SEC1 or PKCS#1 PEM and
as PKCS#12; chains in any order; and the imports that are refused. The
certificates are made and read back with the `cryptography` package."""

import base64
import datetime

import pytest
from azure.core.exceptions import HttpResponseError
from azure.keyvault.certificates import CertificateClient, CertificateContentType, CertificatePolicy
from azure.keyvault.keys import KeyClient
from azure.keyvault.keys.crypto import CryptographyClient, SignatureAlgorithm
from azure.keyvault.secrets import SecretClient
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec, padding, rsa
from cryptography.hazmat.primitives.asymmetric.utils import Prehashed, encode_dss_signature
from cryptography.hazmat.primitives.serialization import pkcs12
from cryptography.x509.oid import NameOID


@pytest.fixture
def certificates(vault, credential):
    return CertificateClient(vault.properties.vault_uri, credential)


def make_certificate(key, subject, issuer_key=None, issuer=None, ca=False):
    now = datetime.datetime.now(datetime.timezone.utc)
    subject_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, subject)])
    issuer_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, issuer)]) if issuer else subject_name
    return (
        x509.CertificateBuilder()
        .subject_name(subject_name)
        .issuer_name(issuer_name)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(minutes=1))
        .not_valid_after(now + datetime.timedelta(days=60))
        .add_extension(x509.BasicConstraints(ca=ca, path_length=None), critical=True)
        .sign(issuer_key or key, hashes.SHA256())
    )


# (key, what the policy reports, a signing algorithm and the hash it signs)
KEYS = {
    "p384": (lambda: ec.generate_private_key(ec.SECP384R1()), ("EC", "P-384", None), (SignatureAlgorithm.es384, hashes.SHA384())),
    "p521": (lambda: ec.generate_private_key(ec.SECP521R1()), ("EC", "P-521", None), (SignatureAlgorithm.es512, hashes.SHA512())),
    "p256k": (lambda: ec.generate_private_key(ec.SECP256K1()), ("EC", "P-256K", None), (SignatureAlgorithm.es256_k, hashes.SHA256())),
    "rsa3072": (lambda: rsa.generate_private_key(public_exponent=65537, key_size=3072), ("RSA", None, 3072), (SignatureAlgorithm.rs256, hashes.SHA256())),
}

FORMATS = {
    "pkcs8-pem": serialization.PrivateFormat.PKCS8,
    # SEC1 ("EC PRIVATE KEY") for EC keys, PKCS#1 ("RSA PRIVATE KEY") for RSA.
    "traditional-pem": serialization.PrivateFormat.TraditionalOpenSSL,
    "pkcs12": None,
}


@pytest.mark.parametrize("encoding", FORMATS)
@pytest.mark.parametrize("kind", KEYS)
def test_imports_by_key_type_and_encoding(certificates, vault, credential, kind, encoding):
    make_key, (family, curve, size), (algorithm, digest_hash) = KEYS[kind]
    key = make_key()
    cert = make_certificate(key, f"{kind}.example.test")
    name = f"{kind}-{encoding}"
    if FORMATS[encoding] is None:
        imported = certificates.import_certificate(name, pkcs12.serialize_key_and_certificates(b"x", key, cert, None, serialization.NoEncryption()))
        content_type = "application/x-pkcs12"
    else:
        pem = key.private_bytes(serialization.Encoding.PEM, FORMATS[encoding], serialization.NoEncryption()) + cert.public_bytes(serialization.Encoding.PEM)
        imported = certificates.import_certificate(name, pem, policy=CertificatePolicy(content_type=CertificateContentType.pem))
        content_type = "application/x-pem-file"

    assert bytes(imported.cer) == cert.public_bytes(serialization.Encoding.DER)
    assert (imported.policy.key_type, imported.policy.key_curve_name, imported.policy.key_size) == (family, curve, size)
    assert imported.policy.content_type == content_type

    # The secret holds the same key.
    secret = SecretClient(vault.properties.vault_uri, credential).get_secret(name)
    assert secret.properties.content_type == content_type
    if content_type == "application/x-pem-file":
        held = serialization.load_pem_private_key(secret.value.encode(), None)
    else:
        held, _, _ = pkcs12.load_key_and_certificates(base64.b64decode(secret.value), None)
    public = serialization.PublicFormat.SubjectPublicKeyInfo
    assert held.public_key().public_bytes(serialization.Encoding.DER, public) == key.public_key().public_bytes(serialization.Encoding.DER, public)

    # And the key signs: verified with the certificate's public key.
    vault_key = KeyClient(vault.properties.vault_uri, credential).get_key(name)
    assert vault_key.key_type == family and (vault_key.key.crv or None) == curve
    digest = hashes.Hash(digest_hash)
    digest.update(b"signed by an imported key")
    digest = digest.finalize()
    signed = CryptographyClient(vault_key, credential).sign(algorithm, digest)
    if family == "EC":
        half = len(signed.signature) // 2
        der = encode_dss_signature(int.from_bytes(signed.signature[:half], "big"), int.from_bytes(signed.signature[half:], "big"))
        cert.public_key().verify(der, digest, ec.ECDSA(Prehashed(digest_hash)))
    else:
        cert.public_key().verify(signed.signature, digest, padding.PKCS1v15(), Prehashed(digest_hash))


def test_chains_in_any_order_and_new_versions(certificates, vault, credential):
    ca_key = ec.generate_private_key(ec.SECP256R1())
    ca = make_certificate(ca_key, "Test CA", ca=True)
    leaf_key = ec.generate_private_key(ec.SECP256R1())
    leaf = make_certificate(leaf_key, "leaf.example.test", issuer_key=ca_key, issuer="Test CA")
    pem = (
        ca.public_bytes(serialization.Encoding.PEM)
        + leaf_key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption())
        + leaf.public_bytes(serialization.Encoding.PEM)
    )
    first = certificates.import_certificate("chained", pem, policy=CertificatePolicy(content_type=CertificateContentType.pem))
    assert bytes(first.cer) == leaf.public_bytes(serialization.Encoding.DER), "the leaf is the certificate of the key, wherever it is"
    secret = SecretClient(vault.properties.vault_uri, credential).get_secret("chained").value
    held = x509.load_pem_x509_certificates(secret.encode())
    assert [c.subject.rfc4514_string() for c in held] == ["CN=leaf.example.test", "CN=Test CA"]

    # PKCS#12 with the CA, imported under the same name: a new version.
    pfx = pkcs12.serialize_key_and_certificates(b"x", leaf_key, leaf, [ca], serialization.BestAvailableEncryption(b"pw"))
    second = certificates.import_certificate("chained", pfx, password="pw")
    assert second.properties.version != first.properties.version
    versions = list(certificates.list_properties_of_certificate_versions("chained"))
    assert len(versions) == 2


def test_refused_imports(certificates):
    key = ec.generate_private_key(ec.SECP256R1())
    other = ec.generate_private_key(ec.SECP256R1())
    cert = make_certificate(key, "mine.example.test")
    pem_policy = CertificatePolicy(content_type=CertificateContentType.pem)

    def refused(value, **options):
        with pytest.raises(HttpResponseError) as error:
            certificates.import_certificate("refused", value, **options)
        assert error.value.status_code == 400, error.value
        return str(error.value)

    mismatched = other.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()) + cert.public_bytes(serialization.Encoding.PEM)
    assert "No certificate matches" in refused(mismatched, policy=pem_policy)
    assert "no private key" in refused(cert.public_bytes(serialization.Encoding.PEM), policy=pem_policy)
    assert "not valid" in refused(b"-----BEGIN CERTIFICATE-----\nnot base64 at all!\n-----END CERTIFICATE-----\n", policy=pem_policy)
    # The SDK sends base64, so bytes that are neither PEM nor an archive fail as PKCS#12.
    assert "could not be read" in refused(b"\xff\xfe not a certificate")
    certificate_only = pkcs12.serialize_key_and_certificates(b"x", None, None, [cert], serialization.NoEncryption())
    assert "no private key" in refused(certificate_only)
    dsa_like = b"-----BEGIN PRIVATE KEY-----\nMAA=\n-----END PRIVATE KEY-----\n" + cert.public_bytes(serialization.Encoding.PEM)
    assert "not an RSA or EC key" in refused(dsa_like, policy=pem_policy)


def test_policy_update_and_pending_operations(certificates):
    from azure.core.exceptions import ResourceNotFoundError

    certificates.begin_create_certificate("site", CertificatePolicy.get_default()).result()
    current = certificates.get_certificate_policy("site")
    policy = CertificatePolicy(issuer_name=current.issuer_name, subject=current.subject, validity_in_months=6, content_type=CertificateContentType.pem)
    updated = certificates.update_certificate_policy("site", policy)
    assert (updated.validity_in_months, updated.content_type) == (6, CertificateContentType.pem)
    assert certificates.get_certificate_policy("site").validity_in_months == 6
    # The next version follows the new policy.
    certificates.begin_create_certificate("site", certificates.get_certificate_policy("site")).result()
    assert certificates.get_certificate("site").policy.content_type == CertificateContentType.pem

    # A finished operation stays readable; a certificate never created has none.
    assert certificates.get_certificate_operation("site").status == "completed"
    with pytest.raises(ResourceNotFoundError):
        certificates.get_certificate_operation("missing")
