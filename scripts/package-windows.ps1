<#
.SYNOPSIS
Build a portable Windows zip for Luban Notepad.

.DESCRIPTION
Builds the release executable (unless -SkipBuild is given), stages it with the
project documentation, and writes a zip plus SHA-256 checksum to dist/.

.PARAMETER SkipBuild
Reuse target\release\luban-notepad.exe instead of running cargo build.
#>
param(
    [switch]$SkipBuild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $root

$versionMatch = Select-String -Path (Join-Path $root 'Cargo.toml') `
    -Pattern '^version = "([^"]+)"' | Select-Object -First 1
if (-not $versionMatch) {
    throw 'could not read version from Cargo.toml'
}
$version = $versionMatch.Matches[0].Groups[1].Value

$targetDir = if ($env:CARGO_TARGET_DIR) {
    $env:CARGO_TARGET_DIR
} else {
    Join-Path $root 'target'
}
$binary = Join-Path $targetDir 'release\luban-notepad.exe'

if (-not $SkipBuild) {
    Write-Host '==> Building release binary'
    cargo build --release -p luban-app
    if ($LASTEXITCODE -ne 0) {
        throw 'cargo build failed'
    }
}

if (-not (Test-Path $binary)) {
    throw "missing release binary at $binary; run without -SkipBuild first"
}

$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    'ARM64' { 'aarch64' }
    default { 'x86_64' }
}

$dist = Join-Path $root 'dist'
$name = "luban-notepad-$version-windows-$arch"
$stage = Join-Path $dist $name
$zip = Join-Path $dist "$name.zip"

Write-Host "==> Staging $name"
if (Test-Path $stage) {
    Remove-Item $stage -Recurse -Force
}
New-Item -ItemType Directory -Path $stage -Force | Out-Null
Copy-Item $binary (Join-Path $stage 'luban-notepad.exe')
$docs = @(
    (Join-Path $root 'README.md'),
    (Join-Path $root 'CHANGELOG.md'),
    (Join-Path $root 'LICENSE'),
    (Join-Path $root 'THIRD_PARTY_LICENSES.md')
)
Copy-Item $docs $stage

Write-Host "==> Creating $name.zip"
if (Test-Path $zip) {
    Remove-Item $zip -Force
}
Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal

$hash = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
"$hash  $name.zip" | Set-Content -Encoding ascii "$zip.sha256"

Write-Host ''
Write-Host 'Artifacts:'
Get-Item $zip, "$zip.sha256" | Format-Table Name, Length -AutoSize
