// A small application: storage with containers, a vault with a secret holding
// the storage key, an identity made by a module, and a resource that is off.
param location string = resourceGroup().location
@minLength(3)
param prefix string
param containerNames array = ['images', 'logs']
param withQueue bool = false
@secure()
param adminSecret string

var accountName = toLower('${prefix}${uniqueString(resourceGroup().id)}')

resource storage 'Microsoft.Storage/storageAccounts@2023-01-01' = {
  name: accountName
  location: location
  sku: { name: 'Standard_LRS' }
  kind: 'StorageV2'
  properties: {
    minimumTlsVersion: 'TLS1_2'
  }
}

resource blobs 'Microsoft.Storage/storageAccounts/blobServices@2023-01-01' = {
  parent: storage
  name: 'default'
}

resource containers 'Microsoft.Storage/storageAccounts/blobServices/containers@2023-01-01' = [for name in containerNames: {
  parent: blobs
  name: name
}]

resource queues 'Microsoft.Storage/storageAccounts/queueServices/queues@2023-01-01' = if (withQueue) {
  name: '${storage.name}/default/work'
}

resource vault 'Microsoft.KeyVault/vaults@2023-07-01' = {
  name: '${prefix}-kv-${take(uniqueString(resourceGroup().id), 6)}'
  location: location
  properties: {
    tenantId: subscription().tenantId
    sku: { family: 'A', name: 'standard' }
    accessPolicies: []
  }
}

resource storageKey 'Microsoft.KeyVault/vaults/secrets@2023-07-01' = {
  parent: vault
  name: 'storage-key'
  properties: {
    value: storage.listKeys().keys[0].value
  }
}

resource admin 'Microsoft.KeyVault/vaults/secrets@2023-07-01' = {
  parent: vault
  name: 'admin'
  properties: {
    value: adminSecret
  }
}

module identity 'identity.bicep' = {
  name: 'identity'
  params: {
    name: '${prefix}-id'
    location: location
  }
}

output blobEndpoint string = storage.properties.primaryEndpoints.blob
output accountName string = storage.name
output vaultUri string = vault.properties.vaultUri
output principalId string = identity.outputs.principalId
output containerIds array = [for (name, i) in containerNames: containers[i].id]
