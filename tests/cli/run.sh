#!/usr/bin/env bash
# Azure CLI smoke test: starts an emulator, registers it as a cloud, signs in
# with a service principal, and drives each service through `az` commands,
# checking what they print.
#
#   tests/cli/run.sh               run it (needs `az` on PATH, or $AZ)
#   tests/cli/run.sh --hosts-line  print the hosts-file line it needs
#
# The CLI resolves per-resource hostnames itself, so they must be in the hosts
# file. $AZURE_EMULATOR_BIN picks the binary (default target/debug), $PORT the
# port, and $EXTRA_CA_BUNDLE adds certificates to trust.
set -euo pipefail

STORE=clismokestore
VAULT=cli-smoke-kv
HOSTS=(
  "$VAULT.vault.azure.localhost"
  "$STORE.blob.core.azure.localhost"
  "$STORE.queue.core.azure.localhost"
  "$STORE.table.core.azure.localhost"
  "$STORE.file.core.azure.localhost"
)
if [[ "${1:-}" == "--hosts-line" ]]; then
  echo "127.0.0.1 ${HOSTS[*]}"
  exit 0
fi

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
az="${AZ:-az}"
binary="${AZURE_EMULATOR_BIN:-$repo/target/debug/azure-emulator}"
port="${PORT:-10444}"
tenant=11111111-1111-1111-1111-111111111111
subscription=00000000-0000-0000-0000-000000000000

for host in "${HOSTS[@]}"; do
  if ! getent hosts "$host" >/dev/null; then
    echo "error: $host does not resolve; add this line to /etc/hosts:" >&2
    echo "  127.0.0.1 ${HOSTS[*]}" >&2
    exit 2
  fi
done

work="$(mktemp -d)"
trap 'kill "$emulator" 2>/dev/null || true; rm -rf "$work"' EXIT
AZURE_EMULATOR_HOME="$work/home" "$binary" --port "$port" --amqp-port 0 --amqps-port 0 > "$work/emulator.log" 2>&1 &
emulator=$!
for _ in $(seq 1 100); do
  [[ -f "$work/home/ca.pem" ]] && curl -sf --cacert "$work/home/ca.pem" -o /dev/null \
    "https://localhost:$port/metadata/endpoints?api-version=2022-09-01" && break
  sleep 0.1
done

# The CLI's own configuration, kept apart from the user's, trusting the
# emulator's certificate authority.
cat "$work/home/ca.pem" ${EXTRA_CA_BUNDLE:-} > "$work/bundle.pem"
export AZURE_CONFIG_DIR="$work/az" REQUESTS_CA_BUNDLE="$work/bundle.pem"
export AZURE_CORE_COLLECT_TELEMETRY=0 AZURE_CORE_NO_COLOR=1 AZURE_CORE_ONLY_SHOW_ERRORS=1
export NO_PROXY="localhost,127.0.0.1,.localhost${NO_PROXY:+,$NO_PROXY}" no_proxy="localhost,127.0.0.1,.localhost${no_proxy:+,$no_proxy}"

failures=0
# check WHAT EXPECTED az ARGS...: runs `az ARGS -o tsv` and compares its output.
check() {
  local what="$1" expected="$2"
  shift 2
  local got
  if ! got="$("$az" "$@" -o tsv 2>"$work/stderr")"; then
    echo "FAIL $what: az $* failed: $(cat "$work/stderr")"
    failures=$((failures + 1))
  elif [[ "$got" != "$expected" ]]; then
    echo "FAIL $what: expected [$expected], got [$got]"
    failures=$((failures + 1))
  else
    echo "ok   $what"
  fi
}
# run WHAT az ARGS...: runs a command whose output is not checked.
run() {
  local what="$1"
  shift
  if "$az" "$@" -o none 2>"$work/stderr"; then
    echo "ok   $what"
  else
    echo "FAIL $what: az $* failed: $(cat "$work/stderr")"
    failures=$((failures + 1))
  fi
}

# ---- signing in ---------------------------------------------------------------
"$az" cloud register -n AzureEmulator --endpoint-resource-manager "https://localhost:$port/" -o none
"$az" cloud set -n AzureEmulator -o none
# Instance discovery would ask login.microsoftonline.com about the authority.
"$az" config set core.instance_discovery=false -o none
"$az" login --service-principal -u cli-smoke -p any-secret --tenant "$tenant" -o none
check "signed in to the emulator's subscription" "$subscription" account show --query id
check "access token for Resource Manager" "Bearer" account get-access-token --query tokenType

# ---- resource groups ----------------------------------------------------------
check "group create" "Succeeded" group create -n cli-rg -l eastus --query properties.provisioningState
check "group exists" "true" group exists -n cli-rg
check "group update tags" "smoke" group update -n cli-rg --tags purpose=smoke --query tags.purpose
check "group list" "cli-rg" group list --query "[?name=='cli-rg'].name"

# ---- storage ------------------------------------------------------------------
check "storage account create" "StorageV2" storage account create -g cli-rg -n "$STORE" -l eastus --sku Standard_LRS --kind StorageV2 --query kind
check "storage account show" "Standard_LRS" storage account show -g cli-rg -n "$STORE" --query sku.name
check "storage account update" "TLS1_2" storage account update -g cli-rg -n "$STORE" --min-tls-version TLS1_2 --query minimumTlsVersion
check "storage account keys" "key1,key2" storage account keys list -g cli-rg -n "$STORE" --query "join(',', [].keyName)"
key="$("$az" storage account keys list -g cli-rg -n "$STORE" --query "[0].value" -o tsv)"
data=(--account-name "$STORE" --account-key "$key")
check "container create" "true" storage container create -n smoke "${data[@]}" --query created
echo "hello from the cli" > "$work/hello.txt"
run "blob upload" storage blob upload -c smoke -n hello.txt -f "$work/hello.txt" "${data[@]}"
check "blob list" "hello.txt" storage blob list -c smoke "${data[@]}" --query "[].name"
run "blob download" storage blob download -c smoke -n hello.txt -f "$work/back.txt" "${data[@]}"
if cmp -s "$work/hello.txt" "$work/back.txt"; then echo "ok   blob round trip"; else echo "FAIL blob round trip"; failures=$((failures + 1)); fi
run "blob delete" storage blob delete -c smoke -n hello.txt "${data[@]}"
check "queue create" "true" storage queue create -n jobs "${data[@]}" --query created
run "queue message put" storage message put -q jobs --content work-item "${data[@]}"
check "queue message get" "work-item" storage message get -q jobs "${data[@]}" --query "[0].content"
check "table create" "true" storage table create -n events "${data[@]}" --query created
run "table entity insert" storage entity insert -t events -e PartitionKey=p RowKey=r1 level=info "${data[@]}"
check "table entity show" "info" storage entity show -t events --partition-key p --row-key r1 "${data[@]}" --query level
check "share create" "true" storage share create -n files "${data[@]}" --query created
check "container through Resource Manager" "smoke" storage container-rm list --storage-account "$STORE" -g cli-rg --query "[].name"

# ---- key vault ----------------------------------------------------------------
check "key vault create" "Succeeded" keyvault create -g cli-rg -n "$VAULT" -l eastus --query properties.provisioningState
check "secret set" "s3cret" keyvault secret set --vault-name "$VAULT" -n db-password --value s3cret --query value
check "secret show" "s3cret" keyvault secret show --vault-name "$VAULT" -n db-password --query value
check "secret list" "db-password" keyvault secret list --vault-name "$VAULT" --query "[].name"
check "key create" "RSA" keyvault key create --vault-name "$VAULT" -n signing --kty RSA --size 2048 --query key.kty
check "key show" "true" keyvault key show --vault-name "$VAULT" -n signing --query "contains(key.kid, '/keys/signing/')"
run "secret delete" keyvault secret delete --vault-name "$VAULT" -n db-password
check "deleted secret list" "db-password" keyvault secret list-deleted --vault-name "$VAULT" --query "[].name"
check "key vault list" "$VAULT" keyvault list -g cli-rg --query "[].name"

# ---- managed identity ---------------------------------------------------------
check "identity create" "eastus" identity create -g cli-rg -n cli-identity --query location
check "identity list" "cli-identity" identity list -g cli-rg --query "[].name"
principal="$("$az" identity show -g cli-rg -n cli-identity --query principalId -o tsv)"
scope="/subscriptions/$subscription/resourceGroups/cli-rg"
check "role assignment create" "Reader" role assignment create --assignee-object-id "$principal" --assignee-principal-type ServicePrincipal --role Reader --scope "$scope" --query "ends_with(roleDefinitionId, 'acdd72a7-3385-48ef-bd42-f606fba81ae7') && 'Reader' || 'other'"
check "role assignment list" "$principal" role assignment list --scope "$scope" --query "[].principalId"
check "role definition" "Reader" role definition list --name Reader --query "[].roleName"
run "role assignment delete" role assignment delete --assignee "$principal" --role Reader --scope "$scope"

# ---- service bus and event hubs -----------------------------------------------
check "service bus namespace create" "Standard" servicebus namespace create -g cli-rg -n cli-smoke-sb -l eastus --sku Standard --query sku.name
check "service bus queue create" "10" servicebus queue create -g cli-rg --namespace-name cli-smoke-sb -n orders --max-delivery-count 10 --query maxDeliveryCount
check "service bus topic and subscription" "audit" servicebus topic subscription create -g cli-rg --namespace-name cli-smoke-sb --topic-name "$("$az" servicebus topic create -g cli-rg --namespace-name cli-smoke-sb -n events --query name -o tsv)" -n audit --query name
check "service bus keys" "RootManageSharedAccessKey" servicebus namespace authorization-rule keys list -g cli-rg --namespace-name cli-smoke-sb -n RootManageSharedAccessKey --query keyName
check "event hubs namespace create" "Standard" eventhubs namespace create -g cli-rg -n cli-smoke-eh -l eastus --sku Standard --query sku.name
check "event hub create" "4" eventhubs eventhub create -g cli-rg --namespace-name cli-smoke-eh -n telemetry --partition-count 4 --query partitionCount
check "consumer group create" "analytics" eventhubs eventhub consumer-group create -g cli-rg --namespace-name cli-smoke-eh --eventhub-name telemetry -n analytics --query name

# ---- cosmos db ----------------------------------------------------------------
check "cosmos account create" "Standard" cosmosdb create -g cli-rg -n cli-smoke-cosmos --locations regionName=eastus --query databaseAccountOfferType
check "cosmos keys" "true" cosmosdb keys list -g cli-rg -n cli-smoke-cosmos --query "length(primaryMasterKey) > \`0\`"
check "cosmos sql database" "shop" cosmosdb sql database create -g cli-rg -a cli-smoke-cosmos -n shop --query name
check "cosmos sql container" "/customer" cosmosdb sql container create -g cli-rg -a cli-smoke-cosmos -d shop -n orders -p /customer --query "resource.partitionKey.paths[0]"

# ---- template deployments -----------------------------------------------------
cat > "$work/template.json" <<'TEMPLATE'
{
  "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
  "contentVersion": "1.0.0.0",
  "parameters": {"name": {"type": "string"}},
  "resources": [{"type": "Microsoft.ManagedIdentity/userAssignedIdentities", "apiVersion": "2023-01-31", "name": "[parameters('name')]", "location": "eastus"}],
  "outputs": {"id": {"type": "string", "value": "[resourceId('Microsoft.ManagedIdentity/userAssignedIdentities', parameters('name'))]"}}
}
TEMPLATE
check "deployment validate" "Succeeded" deployment group validate -g cli-rg -f "$work/template.json" -p name=deployed-identity --query properties.provisioningState
check "deployment create" "Succeeded" deployment group create -g cli-rg -n smoke -f "$work/template.json" -p name=deployed-identity --query properties.provisioningState
check "deployed resource" "deployed-identity" identity show -g cli-rg -n deployed-identity --query name
check "deployment output" "$scope/providers/Microsoft.ManagedIdentity/userAssignedIdentities/deployed-identity" deployment group show -g cli-rg -n smoke --query properties.outputs.id.value

# ---- cleanup ------------------------------------------------------------------
run "storage account delete" storage account delete -g cli-rg -n "$STORE" --yes
run "group delete" group delete -n cli-rg --yes
check "group gone" "false" group exists -n cli-rg

if ((failures)); then
  echo "Azure CLI smoke test: $failures check(s) failed. Emulator log: "
  tail -20 "$work/emulator.log"
  exit 1
fi
echo "Azure CLI smoke test passed."
