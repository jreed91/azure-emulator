"""Entra ID: the token service's discovery documents, signing keys, the v1
and v2 client-credentials endpoints, managed-identity (IMDS) tokens, and the
OAuth errors for malformed requests."""

import base64
import json

import pytest
import requests
from azure.identity import ManagedIdentityCredential
from azure.mgmt.resource.resources import ResourceManagementClient

from conftest import SUBSCRIPTION, TENANT


def claims(token):
    payload = token.split(".")[1]
    return json.loads(base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4)))


def header(token):
    head = token.split(".")[0]
    return json.loads(base64.urlsafe_b64decode(head + "=" * (-len(head) % 4)))


def get(emulator, path, **kwargs):
    return requests.get(f"{emulator.base}{path}", verify=emulator.ca, **kwargs)


def post(emulator, path, data):
    return requests.post(f"{emulator.base}{path}", data=data, verify=emulator.ca)


def test_discovery_documents_and_signing_keys(emulator, credential):
    v2 = get(emulator, f"/{TENANT}/v2.0/.well-known/openid-configuration").json()
    assert v2["token_endpoint"] == f"{emulator.base}/{TENANT}/oauth2/v2.0/token"
    v1 = get(emulator, f"/{TENANT}/.well-known/openid-configuration")
    assert v1.status_code == 200 and v1.json()["token_endpoint"].endswith(f"/{TENANT}/oauth2/token")

    instance = get(emulator, "/common/discovery/instance", params={"api-version": "1.1", "authorization_endpoint": f"{emulator.base}/{TENANT}/oauth2/v2.0/authorize"}).json()
    assert instance["tenant_discovery_endpoint"] == f"{emulator.base}/{TENANT}/v2.0/.well-known/openid-configuration"

    # The keys that sign the tokens are published under both paths.
    kid = header(credential.get_token(emulator.base + "/.default").token)["kid"]
    for path in ("/common/discovery/keys", "/common/discovery/v2.0/keys"):
        assert kid in [k["kid"] for k in get(emulator, path).json()["keys"]]


def test_v1_client_credentials_token_is_accepted_by_resource_manager(emulator):
    response = post(emulator, f"/{TENANT}/oauth2/token", {"grant_type": "client_credentials", "client_id": "v1-app", "client_secret": "s", "resource": emulator.base})
    assert response.status_code == 200
    body = response.json()
    assert body["token_type"] == "Bearer" and int(body["expires_on"]) > int(body["not_before"])
    assert claims(body["access_token"])["appid"] == "v1-app"
    groups = get(emulator, f"/subscriptions/{SUBSCRIPTION}/resourcegroups", params={"api-version": "2021-04-01"}, headers={"Authorization": f"Bearer {body['access_token']}"})
    assert groups.status_code == 200


def test_managed_identity_credential(emulator, monkeypatch):
    # The SDK's documented override of the IMDS endpoint's host.
    monkeypatch.setenv("AZURE_POD_IDENTITY_AUTHORITY_HOST", emulator.base)
    token = ManagedIdentityCredential().get_token(emulator.base + "/.default")
    assert claims(token.token)["aud"].rstrip("/") == emulator.base
    client = ResourceManagementClient(ManagedIdentityCredential(), SUBSCRIPTION, **emulator.arm)
    assert isinstance(list(client.resource_groups.list()), list)


def test_managed_identity_token_for_a_user_assigned_identity(emulator, msi, group):
    identity = msi.user_assigned_identities.create_or_update(group, "imds-app", {"location": "eastus"})
    response = get(
        emulator,
        "/metadata/identity/oauth2/token",
        params={"api-version": "2018-02-01", "resource": emulator.base, "client_id": identity.client_id},
        headers={"Metadata": "true"},
    )
    assert response.status_code == 200
    # Role assignments name a managed identity by its principal id.
    assert claims(response.json()["access_token"])["oid"] == identity.principal_id
    by_object = get(emulator, "/metadata/identity/oauth2/token", params={"resource": emulator.base, "object_id": identity.principal_id}, headers={"Metadata": "true"})
    assert claims(by_object.json()["access_token"])["appid"] == identity.client_id
    by_resource = get(emulator, "/metadata/identity/oauth2/token", params={"resource": emulator.base, "msi_res_id": identity.id}, headers={"Metadata": "true"})
    assert claims(by_resource.json()["access_token"])["oid"] == identity.principal_id
    unknown = get(emulator, "/metadata/identity/oauth2/token", params={"resource": emulator.base, "client_id": "00000000-0000-0000-0000-00000000dead"}, headers={"Metadata": "true"})
    assert unknown.status_code == 400


@pytest.mark.parametrize(
    "params, headers, message",
    [
        ({"api-version": "2018-02-01", "resource": "https://management.azure.com/"}, {}, "Required metadata header not specified"),
        ({"api-version": "2018-02-01"}, {"Metadata": "true"}, "Required query variable 'resource' is missing"),
    ],
)
def test_managed_identity_refusals(emulator, params, headers, message):
    response = get(emulator, "/metadata/identity/oauth2/token", params=params, headers=headers)
    assert response.status_code == 400
    assert response.json() == {"error": "invalid_request", "error_description": message}


@pytest.mark.parametrize(
    "form, status, error, code",
    [
        ({"grant_type": "password", "client_id": "a", "client_secret": "s", "scope": "x/.default"}, 400, "unsupported_grant_type", 70003),
        ({"grant_type": "client_credentials", "client_secret": "s", "scope": "x/.default"}, 400, "invalid_request", 900144),
        ({"grant_type": "client_credentials", "client_id": "a", "scope": "x/.default"}, 401, "invalid_client", 7000216),
        ({"grant_type": "client_credentials", "client_id": "a", "client_secret": "s"}, 400, "invalid_request", 90014),
    ],
    ids=["grant type", "client id", "credential", "scope"],
)
def test_token_request_refusals(emulator, form, status, error, code):
    response = post(emulator, f"/{TENANT}/oauth2/v2.0/token", form)
    assert response.status_code == status
    body = response.json()
    assert (body["error"], body["error_codes"]) == (error, [code])
    assert body["error_description"].startswith(f"AADSTS{code}: ")


def test_unknown_tenant_and_endpoint(emulator):
    unknown = post(emulator, "/22222222-2222-2222-2222-222222222222/oauth2/v2.0/token", {"grant_type": "client_credentials", "client_id": "a", "client_secret": "s", "scope": "x/.default"})
    assert unknown.status_code == 400 and unknown.json()["error"] == "invalid_request"
    assert "not found" in unknown.json()["error_description"]
    # The token endpoint takes only POST.
    wrong_method = get(emulator, f"/{TENANT}/oauth2/v2.0/token")
    assert wrong_method.status_code == 501 and wrong_method.json()["error"] == "not_implemented"
