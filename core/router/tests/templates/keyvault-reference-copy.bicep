param vaultName string
@secure()
param value string

resource copied 'Microsoft.KeyVault/vaults/secrets@2023-07-01' = {
  name: '${vaultName}/copied'
  properties: {
    value: value
  }
}
