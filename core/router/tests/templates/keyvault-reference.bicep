// A module gets a secret from a vault through getSecret(), which compiles to a
// Key Vault reference in the module's parameters, and stores it under a new name.
param vaultName string

resource vault 'Microsoft.KeyVault/vaults@2023-07-01' existing = {
  name: vaultName
}

module copy 'keyvault-reference-copy.bicep' = {
  name: 'copy-secret'
  params: {
    vaultName: vaultName
    value: vault.getSecret('source')
  }
}
