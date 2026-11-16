#!/bin/sh
# Installs the azure-emulator binary from a GitHub release.
#
#   curl -fsSL https://github.com/jreed91/azure-emulator/releases/latest/download/install.sh | sh
#
# Options (or the environment variable after each):
#   --version <v1.2.3>   the release to install           AZURE_EMULATOR_INSTALL_VERSION
#                        (default: the latest)
#   --dir <directory>    where to put the binary          AZURE_EMULATOR_INSTALL_DIR
#                        (default: ~/.local/bin)
#
# AZURE_EMULATOR_INSTALL_URL replaces the release download URL (a mirror, or
# CI testing this script against archives it built).
#
# Supports Linux (x86_64, arm64; glibc) and macOS (Intel, Apple silicon). On
# Windows, use install.ps1. Each download is checked against the release's
# SHA256SUMS.

set -eu

REPO=jreed91/azure-emulator
version=${AZURE_EMULATOR_INSTALL_VERSION:-latest}
dir=${AZURE_EMULATOR_INSTALL_DIR:-"$HOME/.local/bin"}

fail() {
    echo "install.sh: $*" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case $1 in
        --version) [ $# -ge 2 ] || fail "--version needs a value"; version=$2; shift 2 ;;
        --dir) [ $# -ge 2 ] || fail "--dir needs a value"; dir=$2; shift 2 ;;
        -h | --help)
            echo "usage: install.sh [--version <v1.2.3>] [--dir <directory>]"
            exit 0
            ;;
        *) fail "unknown option '$1'" ;;
    esac
done

case $(uname -s) in
    Linux) os=unknown-linux-gnu ;;
    Darwin) os=apple-darwin ;;
    MINGW* | MSYS* | CYGWIN*) fail "on Windows, run install.ps1 in PowerShell" ;;
    *) fail "no release binary for $(uname -s); build from source with cargo (see the README)" ;;
esac
case $(uname -m) in
    x86_64 | amd64) arch=x86_64 ;;
    arm64 | aarch64) arch=aarch64 ;;
    *) fail "no release binary for $(uname -m); build from source with cargo (see the README)" ;;
esac
# A shell under Rosetta reports x86_64 on Apple silicon: install the native binary.
if [ "$os" = apple-darwin ] && [ "$arch" = x86_64 ] &&
    [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
    arch=aarch64
fi
target=$arch-$os
asset=azure-emulator-$target.tar.gz

case $version in
    latest) base=https://github.com/$REPO/releases/latest/download ;;
    v*) base=https://github.com/$REPO/releases/download/$version ;;
    *) base=https://github.com/$REPO/releases/download/v$version ;;
esac
base=${AZURE_EMULATOR_INSTALL_URL:-$base}

if command -v curl > /dev/null 2>&1; then
    download() { curl -fsSL --retry 3 -o "$2" "$1"; }
elif command -v wget > /dev/null 2>&1; then
    download() { wget -q -O "$2" "$1"; }
else
    fail "needs curl or wget"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading $asset ($version)"
download "$base/$asset" "$tmp/$asset" || fail "could not download $base/$asset"
download "$base/SHA256SUMS" "$tmp/SHA256SUMS" || fail "could not download $base/SHA256SUMS"

expected=$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || fail "$asset is not in the release's SHA256SUMS"
if command -v sha256sum > /dev/null 2>&1; then
    actual=$(sha256sum "$tmp/$asset" | awk '{ print $1 }')
else
    actual=$(shasum -a 256 "$tmp/$asset" | awk '{ print $1 }')
fi
[ "$actual" = "$expected" ] || fail "checksum mismatch for $asset"

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$dir"
# Replace, not overwrite in place, so a running emulator keeps its binary.
mv -f "$tmp/azure-emulator" "$dir/azure-emulator"
chmod 755 "$dir/azure-emulator"

echo "Installed $("$dir/azure-emulator" --version) to $dir/azure-emulator"
case ":$PATH:" in
    *":$dir:"*) ;;
    *) echo "Add $dir to your PATH to run it as azure-emulator, e.g.: export PATH=\"$dir:\$PATH\"" ;;
esac
