# A Terraform smoke test: the resources a typical application declares, applied
# with the azurerm provider against the emulator. run.sh drives it.
terraform {
  required_providers {
    azurerm = {
      source  = "hashicorp/azurerm"
      version = "= 5.13.0"
    }
  }
}

variable "metadata_host" {
  description = "The emulator's Resource Manager host and port."
  type        = string
  default     = "localhost:10443"
}

provider "azurerm" {
  features {}
  metadata_host                   = var.metadata_host
  subscription_id                 = "00000000-0000-0000-0000-000000000000"
  tenant_id                       = "11111111-1111-1111-1111-111111111111"
  client_id                       = "terraform"
  client_secret                   = "any-secret"
  resource_provider_registrations = "none"
}

resource "azurerm_resource_group" "smoke" {
  name     = "tf-smoke"
  location = "eastus"
}

data "azurerm_client_config" "current" {}

resource "azurerm_user_assigned_identity" "app" {
  name                = "app-identity"
  location            = azurerm_resource_group.smoke.location
  resource_group_name = azurerm_resource_group.smoke.name
}

resource "azurerm_storage_account" "app" {
  name                     = "tfsmokestore"
  location                 = azurerm_resource_group.smoke.location
  resource_group_name      = azurerm_resource_group.smoke.name
  account_tier             = "Standard"
  account_replication_type = "LRS"
}

resource "azurerm_storage_container" "uploads" {
  name               = "uploads"
  storage_account_id = azurerm_storage_account.app.id
}

resource "azurerm_role_assignment" "blob_reader" {
  scope                = azurerm_storage_account.app.id
  role_definition_name = "Storage Blob Data Reader"
  principal_id         = azurerm_user_assigned_identity.app.principal_id
}

resource "azurerm_key_vault" "app" {
  name                      = "tf-smoke-kv"
  location                  = azurerm_resource_group.smoke.location
  resource_group_name       = azurerm_resource_group.smoke.name
  tenant_id                 = data.azurerm_client_config.current.tenant_id
  sku_name                  = "standard"
  rbac_authorization_enabled = true
}

resource "azurerm_key_vault_secret" "db" {
  name         = "db-password"
  value        = "s3cret"
  key_vault_id = azurerm_key_vault.app.id
}

resource "azurerm_key_vault_key" "signing" {
  name         = "signing"
  key_vault_id = azurerm_key_vault.app.id
  key_type     = "RSA"
  key_size     = 2048
  key_opts     = ["sign", "verify"]
}

resource "azurerm_servicebus_namespace" "app" {
  name                = "tf-smoke-bus"
  location            = azurerm_resource_group.smoke.location
  resource_group_name = azurerm_resource_group.smoke.name
  sku                 = "Standard"
}

resource "azurerm_servicebus_queue" "jobs" {
  name         = "jobs"
  namespace_id = azurerm_servicebus_namespace.app.id
}

resource "azurerm_eventhub_namespace" "app" {
  name                = "tf-smoke-events"
  location            = azurerm_resource_group.smoke.location
  resource_group_name = azurerm_resource_group.smoke.name
  sku                 = "Standard"
}

resource "azurerm_eventhub" "telemetry" {
  name              = "telemetry"
  namespace_id      = azurerm_eventhub_namespace.app.id
  partition_count   = 2
  message_retention = 1
}

output "blob_endpoint" {
  value = azurerm_storage_account.app.primary_blob_endpoint
}

output "vault_uri" {
  value = azurerm_key_vault.app.vault_uri
}
