# Installs the azure-emulator binary from a GitHub release on Windows.
#
#   irm https://github.com/jreed91/azure-emulator/releases/latest/download/install.ps1 | iex
#
# Or, to choose the release or the directory, download it and run:
#
#   .\install.ps1 [-Version v1.2.3] [-Dir <directory>]
#
# The defaults also come from AZURE_EMULATOR_INSTALL_VERSION (the latest
# release when unset) and AZURE_EMULATOR_INSTALL_DIR
# (%LOCALAPPDATA%\Programs\azure-emulator). The directory is added to the
# user's PATH. The download is checked against the release's SHA256SUMS.
# AZURE_EMULATOR_INSTALL_URL replaces the release download URL (a mirror, or
# CI testing this script against archives it built).

param(
    [string]$Version = $(if ($env:AZURE_EMULATOR_INSTALL_VERSION) { $env:AZURE_EMULATOR_INSTALL_VERSION } else { 'latest' }),
    [string]$Dir = $(if ($env:AZURE_EMULATOR_INSTALL_DIR) { $env:AZURE_EMULATOR_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\azure-emulator' })
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$repo = 'jreed91/azure-emulator'

# Windows on Arm runs the x86_64 binary under emulation.
$target = 'x86_64-pc-windows-msvc'
if (-not [Environment]::Is64BitOperatingSystem) {
    throw 'install.ps1: azure-emulator needs 64-bit Windows'
}
$asset = "azure-emulator-$target.zip"

if ($Version -eq 'latest') {
    $base = "https://github.com/$repo/releases/latest/download"
} elseif ($Version.StartsWith('v')) {
    $base = "https://github.com/$repo/releases/download/$Version"
} else {
    $base = "https://github.com/$repo/releases/download/v$Version"
}
if ($env:AZURE_EMULATOR_INSTALL_URL) { $base = $env:AZURE_EMULATOR_INSTALL_URL }

$tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "Downloading $asset ($Version)"
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile (Join-Path $tmp $asset)
    Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS')

    $expected = $null
    foreach ($line in Get-Content (Join-Path $tmp 'SHA256SUMS')) {
        $fields = $line -split '\s+', 2
        if ($fields.Count -eq 2 -and $fields[1].TrimStart('*') -eq $asset) { $expected = $fields[0] }
    }
    if (-not $expected) { throw "install.ps1: $asset is not in the release's SHA256SUMS" }
    $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $tmp $asset)).Hash
    if ($actual -ne $expected) { throw "install.ps1: checksum mismatch for $asset" }

    Expand-Archive -Path (Join-Path $tmp $asset) -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    $exe = Join-Path $Dir 'azure-emulator.exe'
    Move-Item -Force (Join-Path $tmp 'azure-emulator.exe') $exe
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

Write-Host "Installed $(& $exe --version) to $exe"

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$entries = @($userPath -split ';' | Where-Object { $_ })
if ($entries -notcontains $Dir) {
    [Environment]::SetEnvironmentVariable('Path', (($entries + $Dir) -join ';'), 'User')
    $env:Path = "$env:Path;$Dir"
    Write-Host "Added $Dir to your user PATH; open a new terminal to run it as azure-emulator."
}
