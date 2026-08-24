#Requires -Version 5.1
<#
.SYNOPSIS
    Builds the Windows sender and packs it into the per-user installer.

.DESCRIPTION
    Produces dist\RemoteInputBridge-Setup-<version>.exe plus dist\windows-artifact.json, which
    carries the digest the app checks after downloading an update. The release workflow runs this
    same script, so what you build locally is what ships.

    Needs the MSVC Rust toolchain and Inno Setup 6.3+ (winget install JRSoftware.InnoSetup).

.EXAMPLE
    .\scripts\build-windows.ps1
    .\scripts\build-windows.ps1 -SkipBuild          # repack an existing rib-sender.exe
    .\scripts\build-windows.ps1 -ExpectVersion 0.2.0  # fail unless Cargo.toml says 0.2.0
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [string]$ExpectVersion
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$sender = Join-Path $root 'windows-sender'
$dist = Join-Path $root 'dist'

function Get-CargoVersion {
    $line = Select-String -Path (Join-Path $sender 'Cargo.toml') -Pattern '^version = "(.+)"' |
        Select-Object -First 1
    if (-not $line) { throw 'no version in windows-sender/Cargo.toml' }
    return $line.Matches[0].Groups[1].Value
}

function Find-Iscc {
    $candidates = @(
        (Get-Command 'iscc.exe' -ErrorAction SilentlyContinue).Source,
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
    )
    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path $candidate)) { return $candidate }
    }
    throw 'ISCC.exe not found. Install Inno Setup 6.3+ (winget install JRSoftware.InnoSetup).'
}

$version = Get-CargoVersion
if ($ExpectVersion -and $ExpectVersion -ne $version) {
    throw "version mismatch: the tag says $ExpectVersion, windows-sender/Cargo.toml says $version"
}
Write-Host "==> Remote Input Bridge $version"

if (-not $SkipBuild) {
    Write-Host '==> cargo build --release'
    Push-Location $sender
    try {
        & cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
    } finally {
        Pop-Location
    }
}

$exe = Join-Path $sender 'target\release\rib-sender.exe'
if (-not (Test-Path $exe)) { throw "not built: $exe" }

New-Item -ItemType Directory -Force -Path $dist | Out-Null
$iscc = Find-Iscc
Write-Host "==> $iscc"
& $iscc "/DAppVersion=$version" "/DSourceExe=$exe" (Join-Path $root 'installer\windows\rib-setup.iss')
if ($LASTEXITCODE -ne 0) { throw "iscc failed with exit code $LASTEXITCODE" }

$setup = Join-Path $dist "RemoteInputBridge-Setup-$version.exe"
if (-not (Test-Path $setup)) { throw "the installer was not produced: $setup" }

$hash = (Get-FileHash -Algorithm SHA256 -Path $setup).Hash.ToLowerInvariant()
$size = (Get-Item $setup).Length
[ordered]@{
    version = $version
    file    = [System.IO.Path]::GetFileName($setup)
    sha256  = $hash
    size    = $size
} | ConvertTo-Json | Set-Content -Path (Join-Path $dist 'windows-artifact.json') -Encoding UTF8

Write-Host ''
Write-Host "==> done: $setup"
Write-Host "    sha256 $hash"
Write-Host "    $([math]::Round($size / 1MB, 2)) MB"
