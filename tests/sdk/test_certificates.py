"""Key Vault certificates: issued, merged and imported certificates, each with
its key and secret. The certificates and archives are read back with the
`cryptography` package, so they are checked by an independent implementation."""

import base64
import datetime

import pytest
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.keyvault.certificates import (
    CertificateClient,
    CertificateContact,
    CertificateContentType,
    CertificatePolicy,
    KeyCurveName,
    KeyType,
    WellKnownIssuerNames,
)
from azure.keyvault.keys import KeyClient
from azure.keyvault.keys.crypto import CryptographyClient, SignatureAlgorithm
from azure.keyvault.secrets import SecretClient
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec, padding, rsa
from cryptography.hazmat.primitives.serialization import pkcs12
from cryptography.x509.oid import NameOID


@pytest.fixture
def certificates(vault, credential):
    return CertificateClient(vault.properties.vault_uri, credential)


def test_self_signed_certificate_with_key_and_secret(certificates, vault, credential):
    policy = CertificatePolicy(
        issuer_name=WellKnownIssuerNames.self,
        subject="CN=app.example.test, O=Emulator",
        san_dns_names=["app.example.test", "www.example.test"],
        validity_in_months=6,
    )
    created = certificates.begin_create_certificate("app-tls", policy, tags={"env": "test"}).result()
    assert created.properties.tags == {"env": "test"}
    cert = x509.load_der_x509_certificate(bytes(created.cer))
    assert cert.subject.get_attributes_for_oid(NameOID.COMMON_NAME)[0].value == "app.example.test"
    assert cert.extensions.get_extension_for_class(x509.SubjectAlternativeName).value.get_values_for_type(x509.DNSName) == [
        "app.example.test",
        "www.example.test",
    ]
    assert created.properties.x509_thumbprint == cert.fingerprint(hashes.SHA1())
    assert created.properties.expires_on.replace(microsecond=0) == cert.not_valid_after_utc
    # Self-signed: its own key verifies its signature.
    cert.public_key().verify(cert.signature, cert.tbs_certificate_bytes, padding.PKCS1v15(), cert.signature_hash_algorithm)

    # The secret is a PKCS#12 archive with the private key for the certificate.
    secret = SecretClient(vault.properties.vault_uri, credential).get_secret("app-tls")
    assert (secret.properties.content_type, secret.properties.version) == ("application/x-pkcs12", created.properties.version)
    key, leaf, _ = pkcs12.load_key_and_certificates(base64.b64decode(secret.value), None)
    assert leaf == cert and key.public_key().public_numbers() == cert.public_key().public_numbers()

    # The key signs, and the certificate's public key verifies.
    key_bundle = KeyClient(vault.properties.vault_uri, credential).get_key("app-tls")
    assert key_bundle.properties.version == created.properties.version
    digest = hashes.Hash(hashes.SHA256())
    digest.update(b"payload")
    digest = digest.finalize()
    signature = CryptographyClient(key_bundle.id, credential).sign(SignatureAlgorithm.rs256, digest).signature
    cert.public_key().verify(signature, b"payload", padding.PKCS1v15(), hashes.SHA256())

    assert certificates.get_certificate_policy("app-tls").validity_in_months == 6
    assert [c.name for c in certificates.list_properties_of_certificates()] == ["app-tls"]


def test_ec_certificate_as_pem(certificates, vault, credential):
    policy = CertificatePolicy(
        issuer_name=WellKnownIssuerNames.self,
        subject="CN=ec.example.test",
        key_type=KeyType.ec,
        key_curve_name=KeyCurveName.p_384,
        content_type=CertificateContentType.pem,
    )
    created = certificates.begin_create_certificate("ec-cert", policy).result()
    cert = x509.load_der_x509_certificate(bytes(created.cer))
    assert isinstance(cert.public_key(), ec.EllipticCurvePublicKey) and cert.public_key().curve.name == "secp384r1"
    cert.public_key().verify(cert.signature, cert.tbs_certificate_bytes, ec.ECDSA(cert.signature_hash_algorithm))
    pem = SecretClient(vault.properties.vault_uri, credential).get_secret("ec-cert").value
    key = serialization.load_pem_private_key(pem.encode(), None)
    assert x509.load_pem_x509_certificate(pem.encode()) == cert
    assert key.public_key().public_numbers() == cert.public_key().public_numbers()


def test_versions_update_delete_recover_purge(certificates, vault, credential):
    policy = CertificatePolicy.get_default()
    first = certificates.begin_create_certificate("rolling", policy).result()
    second = certificates.begin_create_certificate("rolling", policy).result()
    assert certificates.get_certificate("rolling").properties.version == second.properties.version
    assert certificates.get_certificate_version("rolling", first.properties.version).cer == first.cer
    assert len(list(certificates.list_properties_of_certificate_versions("rolling"))) == 2
    updated = certificates.update_certificate_properties("rolling", enabled=False, tags={"a": "b"})
    assert (updated.properties.enabled, updated.properties.tags) == (False, {"a": "b"})

    deleted = certificates.begin_delete_certificate("rolling").result()
    assert deleted.recovery_id.endswith("/deletedcertificates/rolling")
    with pytest.raises(ResourceNotFoundError):
        certificates.get_certificate("rolling")
    with pytest.raises(ResourceNotFoundError):
        SecretClient(vault.properties.vault_uri, credential).get_secret("rolling")
    assert [c.name for c in certificates.list_deleted_certificates()] == ["rolling"]
    certificates.begin_recover_deleted_certificate("rolling").result()
    assert certificates.get_certificate("rolling").cer == second.cer
    assert KeyClient(vault.properties.vault_uri, credential).get_key("rolling").properties.version == second.properties.version
    certificates.begin_delete_certificate("rolling").wait()
    certificates.purge_deleted_certificate("rolling")
    with pytest.raises(ResourceNotFoundError):
        certificates.get_deleted_certificate("rolling")


def test_import_pkcs12_and_pem(certificates):
    key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "imported.example.test")])
    now = datetime.datetime.now(datetime.timezone.utc)
    cert = (
        x509.CertificateBuilder()
        .subject_name(name)
        .issuer_name(name)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now)
        .not_valid_after(now + datetime.timedelta(days=90))
        .sign(key, hashes.SHA256())
    )
    pfx = pkcs12.serialize_key_and_certificates(b"imported", key, cert, None, serialization.BestAvailableEncryption(b"s3cret"))
    imported = certificates.import_certificate("imported", pfx, password="s3cret")
    assert bytes(imported.cer) == cert.public_bytes(serialization.Encoding.DER)
    assert imported.policy.subject == "CN=imported.example.test"

    pem = key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()) + cert.public_bytes(serialization.Encoding.PEM)
    again = certificates.import_certificate("imported-pem", pem, policy=CertificatePolicy(content_type=CertificateContentType.pem))
    assert again.properties.x509_thumbprint == cert.fingerprint(hashes.SHA1())

    with pytest.raises(HttpResponseError):
        certificates.import_certificate("wrong-password", pfx, password="nope")


def test_unknown_issuer_csr_and_merge(certificates):
    policy = CertificatePolicy(issuer_name=WellKnownIssuerNames.unknown, subject="CN=merged.example.test")
    operation = certificates.begin_create_certificate("merged", policy).result()
    assert operation.status == "inProgress"
    csr = x509.load_der_x509_csr(bytes(certificates.get_certificate_operation("merged").csr))
    assert csr.is_signature_valid and csr.subject.rfc4514_string() == "CN=merged.example.test"

    # Sign the request with a local CA, then merge the result.
    ca_key = ec.generate_private_key(ec.SECP256R1())
    ca_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "Local CA")])
    now = datetime.datetime.now(datetime.timezone.utc)
    signed = (
        x509.CertificateBuilder()
        .subject_name(csr.subject)
        .issuer_name(ca_name)
        .public_key(csr.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now)
        .not_valid_after(now + datetime.timedelta(days=30))
        .sign(ca_key, hashes.SHA256())
    )
    merged = certificates.merge_certificate("merged", [signed.public_bytes(serialization.Encoding.DER)])
    assert bytes(merged.cer) == signed.public_bytes(serialization.Encoding.DER)
    assert certificates.get_certificate_operation("merged").status == "completed"

    # A certificate for another key does not merge.
    certificates.begin_create_certificate("other", policy).result()
    with pytest.raises(HttpResponseError):
        certificates.merge_certificate("other", [signed.public_bytes(serialization.Encoding.DER)])
    cancelled = certificates.cancel_certificate_operation("other")
    assert cancelled.status == "cancelled"
    certificates.delete_certificate_operation("other")
    with pytest.raises(ResourceNotFoundError):
        certificates.get_certificate_operation("other")


def test_backup_restore_contacts_and_issuers(certificates, vault, credential):
    created = certificates.begin_create_certificate("portable", CertificatePolicy.get_default()).result()
    backup = certificates.backup_certificate("portable")
    certificates.begin_delete_certificate("portable").wait()
    certificates.purge_deleted_certificate("portable")
    restored = certificates.restore_certificate_backup(backup)
    assert (restored.cer, restored.properties.version) == (created.cer, created.properties.version)
    secret = SecretClient(vault.properties.vault_uri, credential).get_secret("portable")
    assert pkcs12.load_key_and_certificates(base64.b64decode(secret.value), None)[1] == x509.load_der_x509_certificate(bytes(created.cer))
    with pytest.raises(ResourceExistsError):
        certificates.restore_certificate_backup(backup)

    contacts = certificates.set_contacts([CertificateContact(email="ops@example.test", name="Ops")])
    assert [c.email for c in contacts] == ["ops@example.test"]
    assert [c.name for c in certificates.get_contacts()] == ["Ops"]
    certificates.delete_contacts()
    with pytest.raises(ResourceNotFoundError):
        certificates.get_contacts()

    issuer = certificates.create_issuer("digicert", "DigiCert", account_id="acct", password="pw")
    assert (issuer.provider, issuer.account_id) == ("DigiCert", "acct")
    assert certificates.update_issuer("digicert", account_id="other").account_id == "other"
    assert [i.name for i in certificates.list_properties_of_issuers()] == ["digicert"]
    certificates.delete_issuer("digicert")
    with pytest.raises(ResourceNotFoundError):
        certificates.get_issuer("digicert")
