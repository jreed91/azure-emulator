// Bicep compiles user-defined types and functions to languageVersion 2.0, where
// resources are keyed by symbolic name.
type tier = 'Basic' | 'Standard'

func namespaceName(prefix string) string => '${prefix}-bus-${uniqueString(resourceGroup().id)}'

param prefix string
param sku tier = 'Standard'
param queues array = ['orders', 'invoices']
param accountName string

resource account 'Microsoft.Storage/storageAccounts@2023-01-01' existing = {
  name: accountName
}

resource bus 'Microsoft.ServiceBus/namespaces@2021-11-01' = {
  name: namespaceName(prefix)
  location: resourceGroup().location
  sku: { name: sku, tier: sku }
  tags: { storage: account.id }
}

resource queue 'Microsoft.ServiceBus/namespaces/queues@2021-11-01' = [for (q, i) in queues: {
  parent: bus
  name: q
  properties: {
    maxDeliveryCount: 5 + i
    lockDuration: 'PT1M'
  }
}]

output endpoint string = bus.properties.serviceBusEndpoint
output blob string = account.properties.primaryEndpoints.blob
output queueNames array = [for i in range(0, length(queues)): queue[i].name]
output upper array = map(queues, q => toUpper(q))
output picked array = filter(queues, q => startsWith(q, 'o'))
