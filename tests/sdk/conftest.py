"""Fixtures that run the emulator and point the official Azure SDKs at it.

Each pytest process (each xdist worker) starts its own emulator on a free port
with its own state directory, so tests never share state with a developer's
running emulator. Tests create their own resource group and uniquely named
resources, so they are independent of each other and of their order.

The emulator binary is ``$AZURE_EMULATOR_BIN``, or ``target/debug/azure-emulator``
in the repository; build it first with ``cargo build -p azure-emulator``.

With ``AZURE_EMULATOR_LIVE=1`` the suite runs against a real Azure subscription
instead (``AZURE_SUBSCRIPTION_ID``, ``AZURE_TENANT_ID``, and optionally
``AZURE_LOCATION``), signed in with ``DefaultAzureCredential``. Only tests marked
``@pytest.mark.live`` run then: the ones whose assertions hold on Azure as
written, so that a failure is a difference between the emulator and Azure. See
``.github/workflows/live.yml`` for the roles the signed-in principal needs.
"""

import contextlib
import os
import pathlib
import socket
import ssl
import subprocess
import time
import urllib.request
import uuid

import pytest
from azure.identity import ClientSecretCredential

LIVE = os.environ.get("AZURE_EMULATOR_LIVE", "") not in ("", "0")
SUBSCRIPTION = os.environ["AZURE_SUBSCRIPTION_ID"] if LIVE else "00000000-0000-0000-0000-000000000000"
TENANT = os.environ["AZURE_TENANT_ID"] if LIVE else "11111111-1111-1111-1111-111111111111"
LOCATION = os.environ.get("AZURE_LOCATION", "eastus") if LIVE else "eastus"
# Resource groups made by a live run carry this tag, so that CI can delete any
# a cancelled run left behind.
LIVE_TAG = "azure-emulator-live"
REPO = pathlib.Path(__file__).resolve().parents[2]

# Data planes live on per-resource hostnames under *.localhost. Not every
# resolver answers for those, so resolve them to the loopback address here.
_getaddrinfo = socket.getaddrinfo


def _resolve_localhost(host, *args, **kwargs):
    if isinstance(host, str) and host.endswith(".localhost"):
        host = "127.0.0.1"
    return _getaddrinfo(host, *args, **kwargs)


socket.getaddrinfo = _resolve_localhost


def _free_port():
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def _wait_until_ready(base, ca_path, process, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"the emulator exited with status {process.returncode}")
        if ca_path.exists():
            try:
                context = ssl.create_default_context(cafile=str(ca_path))
                opener = urllib.request.build_opener(
                    urllib.request.ProxyHandler({}), urllib.request.HTTPSHandler(context=context)
                )
                opener.open(f"{base}/metadata/endpoints?api-version=2022-09-01", timeout=2).read()
                return
            except OSError:
                pass
        time.sleep(0.1)
    raise RuntimeError("the emulator did not become ready in time")


class Emulator:
    def __init__(self, base, port, amqp_port, amqps_port, home):
        self.base = base
        self.port = port
        self.amqp_port = amqp_port
        self.amqps_port = amqps_port
        self.home = home
        self.ca = str(home / "ca.pem")
        self.tenant = TENANT
        self.subscription = SUBSCRIPTION

    @property
    def arm(self):
        """Keyword arguments that point a management client at the emulator."""
        return {"base_url": self.base, "credential_scopes": [self.base + "/.default"]}


@contextlib.contextmanager
def run_emulator(home, amqps_port=None, extra_args=(), port=None, amqp_port=None):
    """Runs an emulator process with its state and CA in `home`, on free
    ports unless they are given (and TLS for Service Bus on `amqps_port`)."""
    binary = pathlib.Path(os.environ.get("AZURE_EMULATOR_BIN", REPO / "target/debug/azure-emulator"))
    if not binary.exists():
        pytest.exit(f"emulator binary not found at {binary}; run `cargo build -p azure-emulator` first", 2)
    port = port or _free_port()
    amqp_port = amqp_port or _free_port()
    amqps_port = amqps_port or _free_port()
    base = f"https://localhost:{port}"
    log = open(home / f"emulator-{port}.log", "w")
    process = subprocess.Popen(
        [str(binary), "--port", str(port), "--amqp-port", str(amqp_port), "--amqps-port", str(amqps_port), *extra_args],
        env={**os.environ, "AZURE_EMULATOR_HOME": str(home)},
        stdout=log,
        stderr=subprocess.STDOUT,
    )
    try:
        _wait_until_ready(base, home / "ca.pem", process)
        yield Emulator(base, port, amqp_port, amqps_port, home)
    finally:
        process.terminate()
        process.wait(timeout=10)
        log.close()


def pytest_configure(config):
    config.addinivalue_line("markers", "live: also runs against a real Azure subscription (AZURE_EMULATOR_LIVE=1)")


def pytest_collection_modifyitems(config, items):
    if not LIVE:
        return
    skip = pytest.mark.skip(reason="emulator only; not marked live")
    for item in items:
        if "live" not in item.keywords:
            item.add_marker(skip)


class LiveAzure:
    """Stands in for `Emulator` when the suite runs against Azure: management
    clients take their default endpoints."""

    tenant = TENANT
    subscription = SUBSCRIPTION
    arm = {}


@pytest.fixture(scope="session")
def emulator(tmp_path_factory):
    if LIVE:
        yield LiveAzure()
        return
    home = tmp_path_factory.mktemp("emulator-home")
    with run_emulator(home) as running:
        # The SDKs read these when their first client is built.
        os.environ["REQUESTS_CA_BUNDLE"] = running.ca
        no_proxy = os.environ.get("NO_PROXY", "")
        os.environ["NO_PROXY"] = os.environ["no_proxy"] = ",".join(
            part for part in [no_proxy, "localhost", "127.0.0.1", ".localhost"] if part
        )
        yield running


@pytest.fixture(scope="session")
def credential(emulator):
    if LIVE:
        from azure.identity import DefaultAzureCredential

        return DefaultAzureCredential(exclude_managed_identity_credential=True)
    # Any client id and secret are accepted.
    return ClientSecretCredential(
        TENANT, "sdk-tests", "any-secret", authority=emulator.base, disable_instance_discovery=True
    )


def unique(prefix, length=10):
    """A name no other test uses: lowercase letters and digits after `prefix`."""
    return prefix + uuid.uuid4().hex[:length]


@pytest.fixture(scope="session")
def resources(emulator, credential):
    from azure.mgmt.resource.resources import ResourceManagementClient

    return ResourceManagementClient(credential, SUBSCRIPTION, **emulator.arm)


@pytest.fixture(scope="session")
def deployments(emulator, credential):
    from azure.mgmt.resource.deployments import DeploymentsMgmtClient

    return DeploymentsMgmtClient(credential, SUBSCRIPTION, **emulator.arm)


@pytest.fixture(scope="session")
def authorization(emulator, credential):
    from azure.mgmt.authorization import AuthorizationManagementClient

    return AuthorizationManagementClient(credential, SUBSCRIPTION, **emulator.arm)


@pytest.fixture(scope="session")
def storage(emulator, credential):
    from azure.mgmt.storage import StorageManagementClient

    return StorageManagementClient(credential, SUBSCRIPTION, **emulator.arm)


@pytest.fixture(scope="session")
def keyvault(emulator, credential):
    from azure.mgmt.keyvault import KeyVaultManagementClient

    return KeyVaultManagementClient(credential, SUBSCRIPTION, **emulator.arm)


@pytest.fixture(scope="session")
def cosmos(emulator, credential):
    from azure.mgmt.cosmosdb import CosmosDBManagementClient

    return CosmosDBManagementClient(credential, SUBSCRIPTION, **emulator.arm)


@pytest.fixture(scope="session")
def eventhub(emulator, credential):
    from azure.mgmt.eventhub import EventHubManagementClient

    return EventHubManagementClient(credential, SUBSCRIPTION, **emulator.arm)


@pytest.fixture(scope="session")
def servicebus(emulator, credential):
    from azure.mgmt.servicebus import ServiceBusManagementClient

    return ServiceBusManagementClient(credential, SUBSCRIPTION, **emulator.arm)


@pytest.fixture(scope="session")
def msi(emulator, credential):
    from azure.mgmt.msi import ManagedServiceIdentityClient

    return ManagedServiceIdentityClient(credential, SUBSCRIPTION, **emulator.arm)


@pytest.fixture
def group(resources):
    """A fresh resource group, deleted (with everything in it) afterwards."""
    name = unique("rg-")
    if LIVE:
        # Not waited for: deleting a group on Azure takes minutes.
        resources.resource_groups.create_or_update(name, {"location": LOCATION, "tags": {LIVE_TAG: os.environ.get("GITHUB_RUN_ID", "local")}})
        yield name
        resources.resource_groups.begin_delete(name)
        return
    resources.resource_groups.create_or_update(name, {"location": "eastus"})
    yield name
    if resources.resource_groups.check_existence(name):
        resources.resource_groups.begin_delete(name).result()


class StorageAccount:
    def __init__(self, name, blob_url, key, queue_url=None, table_url=None, file_url=None):
        self.name = name
        self.blob_url = blob_url
        self.queue_url = queue_url
        self.table_url = table_url
        self.file_url = file_url
        self.key = key


@pytest.fixture
def account(storage, group):
    """A StorageV2 account in a fresh group, with its blob endpoint and first key."""
    name = unique("st")
    created = storage.storage_accounts.begin_create(
        group, name, {"location": LOCATION, "kind": "StorageV2", "sku": {"name": "Standard_LRS"}}
    ).result()
    key = storage.storage_accounts.list_keys(group, name)["keys"][0]["value"]
    return StorageAccount(name, created.primary_endpoints.blob, key, created.primary_endpoints.queue, created.primary_endpoints.table, created.primary_endpoints.file)


@pytest.fixture
def vault(keyvault, group):
    """A Key Vault vault in a fresh group."""
    name = unique("kv-")
    if LIVE:
        # Data-plane access through the signed-in principal's role assignments.
        properties = {
            "tenantId": TENANT,
            "sku": {"family": "A", "name": "standard"},
            "enableRbacAuthorization": True,
            "softDeleteRetentionInDays": 7,
        }
        return keyvault.vaults.begin_create_or_update(group, name, {"location": LOCATION, "properties": properties}).result()
    return keyvault.vaults.begin_create_or_update(
        group,
        name,
        {"location": "eastus", "properties": {"tenantId": TENANT, "sku": {"family": "A", "name": "standard"}, "accessPolicies": []}},
    ).result()
