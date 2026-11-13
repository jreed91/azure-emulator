// A subscription deployment that creates a group and deploys into it.
targetScope = 'subscription'

param groupName string
param location string = deployment().location

resource group 'Microsoft.Resources/resourceGroups@2021-04-01' = {
  name: groupName
  location: location
  tags: { owner: 'templates' }
}

module identity 'identity.bicep' = {
  name: 'identity-in-group'
  scope: group
  params: {
    name: 'sub-made-id'
    location: location
  }
}

output groupId string = group.id
output clientId string = identity.outputs.clientId
