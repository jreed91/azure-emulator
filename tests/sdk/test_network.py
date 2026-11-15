"""Microsoft.Network, served from its schema: settings are checked, stored and
returned. Nothing is allocated or connected, so these tests assert only what a
caller sends and gets back. Requests are written as the REST API takes them."""

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError

from conftest import unique


def vnet_body(prefix):
    return {"location": "eastus", "properties": {"addressSpace": {"addressPrefixes": [prefix]}}}


def subnet_body(prefix):
    return {"properties": {"addressPrefix": prefix}}


def rule(access, priority, port):
    return {
        "protocol": "Tcp",
        "access": access,
        "direction": "Inbound",
        "priority": priority,
        "sourceAddressPrefix": "*",
        "sourcePortRange": "*",
        "destinationAddressPrefix": "*",
        "destinationPortRange": port,
    }


def test_virtual_network_and_subnets(network, group):
    name = unique("vnet-")
    vnet = network.virtual_networks.begin_create_or_update(group, name, {**vnet_body("10.0.0.0/16"), "tags": {"env": "test"}}).result()
    assert vnet.provisioning_state == "Succeeded"
    assert vnet.address_space.address_prefixes == ["10.0.0.0/16"]
    assert vnet.id.endswith(f"/providers/Microsoft.Network/virtualNetworks/{name}")

    subnet = network.subnets.begin_create_or_update(group, name, "apps", subnet_body("10.0.1.0/24")).result()
    assert (subnet.name, subnet.address_prefix) == ("apps", "10.0.1.0/24")
    assert network.subnets.get(group, name, "apps").address_prefix == "10.0.1.0/24"
    assert [s.name for s in network.subnets.list(group, name)] == ["apps"]

    assert network.virtual_networks.update_tags(group, name, {"tags": {"env": "prod"}}).tags == {"env": "prod"}
    assert name in [v.name for v in network.virtual_networks.list(group)]
    assert name in [v.name for v in network.virtual_networks.list_all()]

    network.subnets.begin_delete(group, name, "apps").result()
    with pytest.raises(ResourceNotFoundError):
        network.subnets.get(group, name, "apps")
    network.virtual_networks.begin_delete(group, name).result()
    with pytest.raises(ResourceNotFoundError):
        network.virtual_networks.get(group, name)


def test_security_group_public_address_and_interface(network, group):
    nsg = network.network_security_groups.begin_create_or_update(
        group, "web", {"location": "eastus", "properties": {"securityRules": [{"name": "https", "properties": rule("Allow", 100, "443")}]}}
    ).result()
    assert [(r.name, r.priority, r.destination_port_range) for r in nsg.security_rules] == [("https", 100, "443")]
    ssh = network.security_rules.begin_create_or_update(group, "web", "ssh", {"properties": rule("Deny", 200, "22")}).result()
    assert (ssh.access, ssh.priority) == ("Deny", 200)

    address = network.public_ip_addresses.begin_create_or_update(
        group, "front", {"location": "eastus", "sku": {"name": "Standard"}, "properties": {"publicIPAllocationMethod": "Static"}}
    ).result()
    assert (address.sku.name, address.public_ip_allocation_method) == ("Standard", "Static")

    network.virtual_networks.begin_create_or_update(group, "net", vnet_body("10.1.0.0/16")).result()
    subnet = network.subnets.begin_create_or_update(group, "net", "default", subnet_body("10.1.0.0/24")).result()
    configuration = {"name": "primary", "properties": {"subnet": {"id": subnet.id}, "publicIPAddress": {"id": address.id}}}
    nic = network.network_interfaces.begin_create_or_update(
        group, "nic", {"location": "eastus", "properties": {"networkSecurityGroup": {"id": nsg.id}, "ipConfigurations": [configuration]}}
    ).result()
    # References come back as sent.
    returned = nic.ip_configurations[0]
    assert (returned.subnet.id, returned.public_ip_address.id) == (subnet.id, address.id)
    assert nic.network_security_group.id == nsg.id


def test_settings_are_checked_against_the_specification(network, group):
    with pytest.raises(HttpResponseError) as refused:
        network.public_ip_addresses.begin_create_or_update(
            group, "bad", {"location": "eastus", "properties": {"idleTimeoutInMinutes": "soon"}}
        ).result()
    assert refused.value.status_code == 400
    with pytest.raises(HttpResponseError) as refused:
        network.virtual_networks.begin_create_or_update(group, "nowhere", {"properties": {"addressSpace": {"addressPrefixes": ["10.0.0.0/16"]}}}).result()
    assert refused.value.error.code == "LocationRequired"


@pytest.mark.known_gap("addresses are not allocated: a dynamic private address is never assigned")
def test_interface_gets_a_private_address(network, group):
    network.virtual_networks.begin_create_or_update(group, "net", vnet_body("10.2.0.0/16")).result()
    subnet = network.subnets.begin_create_or_update(group, "net", "default", subnet_body("10.2.0.0/24")).result()
    configuration = {"name": "primary", "properties": {"subnet": {"id": subnet.id}}}
    nic = network.network_interfaces.begin_create_or_update(
        group, "nic", {"location": "eastus", "properties": {"ipConfigurations": [configuration]}}
    ).result()
    assert nic.ip_configurations[0].private_ip_address.startswith("10.2.0.")


@pytest.mark.known_gap("a virtual network does not list the subnets created inside it")
def test_virtual_network_lists_its_subnets(network, group):
    network.virtual_networks.begin_create_or_update(group, "net", vnet_body("10.3.0.0/16")).result()
    network.subnets.begin_create_or_update(group, "net", "default", subnet_body("10.3.0.0/24")).result()
    assert [s.name for s in network.virtual_networks.get(group, "net").subnets] == ["default"]
