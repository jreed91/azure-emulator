// The usual pattern: an identity gets a data role on one storage account.
param location string = resourceGroup().location

var blobReader = subscriptionResourceId('Microsoft.Authorization/roleDefinitions', '2a2b9908-6ea1-4ae2-8e65-a410df84e7d1')

resource identity 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = {
  name: 'reader-id'
  location: location
}

resource storage 'Microsoft.Storage/storageAccounts@2023-01-01' = {
  name: 'roles${uniqueString(resourceGroup().id)}'
  location: location
  sku: { name: 'Standard_LRS' }
  kind: 'StorageV2'
}

resource readBlobs 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(storage.id, identity.id, blobReader)
  scope: storage
  properties: {
    roleDefinitionId: blobReader
    principalId: identity.properties.principalId
    principalType: 'ServicePrincipal'
  }
}

output assignmentId string = readBlobs.id
output principalId string = identity.properties.principalId
