#!/usr/bin/env bash
# Terraform smoke test: starts an emulator, then applies main.tf with the azurerm
# provider, checks that a second plan finds nothing to change, and destroys it.
#
#   tests/terraform/run.sh              run it (needs `terraform` on PATH, or $TERRAFORM)
#   tests/terraform/run.sh --hosts-line print the hosts-file line it needs
#
# The provider resolves per-resource hostnames itself, so they must be in the
# hosts file. $AZURE_EMULATOR_BIN picks the binary (default target/debug), and
# $EXTRA_CA_BUNDLE adds certificates to trust (behind a TLS-inspecting proxy).
set -euo pipefail

HOSTS=(
  tf-smoke-kv.vault.azure.localhost
  tfsmokestore.blob.core.azure.localhost
  tfsmokestore.queue.core.azure.localhost
  tfsmokestore.table.core.azure.localhost
  tfsmokestore.file.core.azure.localhost
  tfsmokestore.dfs.core.azure.localhost
  tfsmokestore.web.core.azure.localhost
)
if [[ "${1:-}" == "--hosts-line" ]]; then
  echo "127.0.0.1 ${HOSTS[*]}"
  exit 0
fi

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
terraform="${TERRAFORM:-terraform}"
binary="${AZURE_EMULATOR_BIN:-$repo/target/debug/azure-emulator}"
port="${PORT:-10443}"

for host in "${HOSTS[@]}"; do
  if ! getent hosts "$host" >/dev/null; then
    echo "error: $host does not resolve; add this line to /etc/hosts:" >&2
    echo "  127.0.0.1 ${HOSTS[*]}" >&2
    exit 2
  fi
done

work="$(mktemp -d)"
trap 'kill "$emulator" 2>/dev/null || true; rm -rf "$work"' EXIT
cp "$here/main.tf" "$here/.terraform.lock.hcl" "$work/"

AZURE_EMULATOR_HOME="$work/home" "$binary" --port "$port" > "$work/emulator.log" 2>&1 &
emulator=$!
for _ in $(seq 1 100); do
  [[ -f "$work/home/ca.pem" ]] && curl -sf --cacert "$work/home/ca.pem" -o /dev/null \
    "https://localhost:$port/metadata/endpoints?api-version=2022-09-01" && break
  sleep 0.1
done

# Go reads one bundle: the system's roots (to download the provider) and the
# emulator's certificate authority.
system=""
for candidate in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt /etc/ssl/cert.pem; do
  [[ -f "$candidate" ]] && system="$candidate" && break
done
cat $system ${EXTRA_CA_BUNDLE:-} "$work/home/ca.pem" > "$work/bundle.pem"
export SSL_CERT_FILE="$work/bundle.pem"
export NO_PROXY="localhost,127.0.0.1,.localhost${NO_PROXY:+,$NO_PROXY}" no_proxy="localhost,127.0.0.1,.localhost${no_proxy:+,$no_proxy}"
export TF_IN_AUTOMATION=1 TF_INPUT=0

cd "$work"
var=(-var "metadata_host=localhost:$port")
"$terraform" init -no-color >/dev/null
"$terraform" apply -auto-approve -no-color "${var[@]}"
# Everything must read back as it was written: a second plan changes nothing.
"$terraform" plan -detailed-exitcode -no-color "${var[@]}"
"$terraform" destroy -auto-approve -no-color "${var[@]}"
echo "Terraform smoke test passed."
