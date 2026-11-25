#!/usr/bin/env bash
# Azure Functions host smoke test: runs the Functions v4 host (its Docker image,
# with a .NET isolated app) against the emulator's path-style storage ports, the
# way a project moving from Azurite would, and checks that the host's own use of
# AzureWebJobsStorage works: lock leases, keys in azure-webjobs-secrets, timer,
# queue (with poison handling) and blob triggers, and blob, queue and table
# outputs.
#
#   tests/functions/run.sh
#
# Needs Docker (with host networking) and the JavaScript SDK test dependencies
# (`cd tests/sdk-js && npm ci`). $AZURE_EMULATOR_BIN picks the binary (default
# target/debug); $EXTRA_CA_BUNDLE adds certificates the .NET build trusts (for a
# proxy in front of NuGet), and HTTPS_PROXY is passed to it when set.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
bin="${AZURE_EMULATOR_BIN:-$repo/target/debug/azure-emulator}"
host_image="${FUNCTIONS_IMAGE:-mcr.microsoft.com/azure-functions/dotnet-isolated:4-dotnet-isolated8.0}"
sdk_image="${DOTNET_SDK_IMAGE:-mcr.microsoft.com/dotnet/sdk:8.0}"
work="$(mktemp -d)"
container="azure-emulator-functions-$$"
export NO_PROXY="127.0.0.1,localhost${NO_PROXY:+,$NO_PROXY}" no_proxy="127.0.0.1,localhost${no_proxy:+,$no_proxy}"

cleanup() {
  docker rm -f "$container" >/dev/null 2>&1 || true
  [[ -n "${emulator:-}" ]] && kill "$emulator" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

echo "Building the function app"
cp -r "$here/app" "$work/app"
build_args=(--rm -v "$work/app:/src" -w /src -e DOTNET_CLI_TELEMETRY_OPTOUT=1)
script="dotnet publish -c Release -o /src/out >/src/build.log 2>&1 || { cat /src/build.log; exit 1; }"
if [[ -n "${EXTRA_CA_BUNDLE:-}" ]]; then
  build_args+=(-v "$EXTRA_CA_BUNDLE:/extra-ca.crt:ro")
  script="cp /extra-ca.crt /usr/local/share/ca-certificates/extra.crt && update-ca-certificates >/dev/null 2>&1; $script"
fi
if [[ -n "${HTTPS_PROXY:-}" ]]; then
  build_args+=(--network host -e "HTTPS_PROXY=$HTTPS_PROXY")
fi
docker run "${build_args[@]}" "$sdk_image" bash -c "$script"

echo "Starting the emulator"
cat >"$work/config.json" <<'JSON'
{ "resources": [
  { "id": "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/local", "apiVersion": "2021-04-01", "body": { "location": "eastus" } },
  { "id": "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/local/providers/Microsoft.Storage/storageAccounts/devstoreaccount1",
    "apiVersion": "2023-05-01", "body": { "location": "eastus", "kind": "StorageV2", "sku": { "name": "Standard_LRS" } } } ] }
JSON
port=${PORT:-10443}
AZURE_EMULATOR_HOME="$work/home" "$bin" --http --port "$port" --config "$work/config.json" \
  --blob-port 10000 --queue-port 10001 --table-port 10002 >"$work/emulator.log" 2>&1 &
emulator=$!
base="http://localhost:$port"
for _ in $(seq 100); do curl -sf "$base/__admin/health" >/dev/null && break; sleep 0.1; done
token=$(curl -sf "$base/11111111-1111-1111-1111-111111111111/oauth2/v2.0/token" \
  -d "grant_type=client_credentials&client_id=a&client_secret=b&scope=$base/.default" | python3 -c 'import json,sys; print(json.load(sys.stdin)["access_token"])')
key=$(curl -sf -X POST -H "authorization: Bearer $token" -H "content-length: 0" \
  "$base/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/local/providers/Microsoft.Storage/storageAccounts/devstoreaccount1/listKeys?api-version=2023-05-01" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["keys"][0]["value"])')
storage() { echo "DefaultEndpointsProtocol=http;AccountName=devstoreaccount1;AccountKey=$key;BlobEndpoint=http://$1:10000/devstoreaccount1;QueueEndpoint=http://$1:10001/devstoreaccount1;TableEndpoint=http://$1:10002/devstoreaccount1"; }

echo "Starting the Functions host"
# `azure` stands for the emulator's service name in Compose: any hostname works.
docker run -d --name "$container" --network host --add-host azure:127.0.0.1 \
  -e AzureWebJobsStorage="$(storage azure)" -e AzureWebJobsSecretStorageType=blob \
  -e FUNCTIONS_WORKER_RUNTIME=dotnet-isolated -e AZURE_FUNCTIONS_ENVIRONMENT=Development \
  -e ASPNETCORE_URLS=http://+:7071 -e AzureFunctionsJobHost__logging__console__isEnabled=true \
  -v "$work/app/out:/home/site/wwwroot:ro" "$host_image" >/dev/null
wait_for() {
  for _ in $(seq 120); do docker logs "$container" 2>&1 | grep -q "$1" && return 0; sleep 1; done
  docker logs "$container" 2>&1 | tail -50
  echo "the host never logged: $1" >&2
  return 1
}
wait_for "Job host started"

local_storage="$(storage 127.0.0.1)"
# A request with a wrong key makes the host create its keys.
curl -s -o /dev/null -X POST "http://127.0.0.1:7071/api/Enqueue?id=x&code=wrong"
code=$(node "$here/check.mjs" secrets "$local_storage")
for id in one two poison1; do
  curl -sf -o /dev/null -X POST "http://127.0.0.1:7071/api/Enqueue?id=$id&code=$code"
done
node "$here/check.mjs" upload "$local_storage"
wait_for "Executed 'Functions.OnBlob' (Succeeded"
for _ in $(seq 60); do
  [[ $(docker logs "$container" 2>&1 | grep -c "Executed 'Functions.Work' (Failed") -ge 5 ]] && break
  sleep 1
done
sleep 3
[[ "$(curl -sf "http://127.0.0.1:7071/api/read/one?code=$code")" == "processed one" ]] || { echo "Read did not return the blob" >&2; exit 1; }
node "$here/check.mjs" check "$local_storage"
# A listener that failed to start, or a storage error the SDKs could not read.
if docker logs "$container" 2>&1 | grep -E "fail: Host.Startup|XmlException|RequestFailedException"; then
  echo "the host logged storage failures" >&2
  exit 1
fi
echo "Functions host smoke test passed"
