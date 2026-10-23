<#
.SYNOPSIS
Build the Windows portable zip and installer for Luban Notepad.

.DESCRIPTION
Builds the release executable (unless -SkipBuild), stages the portable
distribution as a zip with a SHA-256 checksum, and compiles an Inno Setup
installer. Artifacts are written to dist/.

.PARAMETER SkipBuild
Reuse target\release\luban-notepad.exe instead of running cargo build.

.PARAMETER SkipPortable
Do not create the portable zip.

.PARAMETER SkipInstaller
Do not compile the Inno Setup installer.

.PARAMETER InnoSetupPath
Path to ISCC.exe. Defaults to $env:INNO_SETUP_ISCC or the usual install
locations.
#>
param(
    [switch]$SkipBuild,
    [switch]$SkipPortable,
    [switch]$SkipInstaller,
    [string]$InnoSetupPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Write-Checksum([string]$Path) {
    $hash = (Get-FileHash -Algorithm SHA256 $Path).Hash.ToLower()
    $name = Split-Path $Path -Leaf
    Set-Content -Path "${Path}.sha256" -Value "$hash  $name" -Encoding ascii
}

function Find-Iscc([string]$Explicit) {
    if ($Explicit) {
        if (-not (Test-Path $Explicit)) {
            throw "ISCC.exe not found at $Explicit"
        }
        return (Resolve-Path $Explicit).Path
    }
    if ($env:INNO_SETUP_ISCC -and (Test-Path $env:INNO_SETUP_ISCC)) {
        return (Resolve-Path $env:INNO_SETUP_ISCC).Path
    }
    $candidates = @()
    if (${env:ProgramFiles(x86)}) {
        $candidates += Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'
    }
    if ($env:ProgramFiles) {
        $candidates += Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe'
    }
    if ($env:LOCALAPPDATA) {
        $candidates += Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe'
    }
    foreach ($candidate in $candidates) {
        if (Test-Path $candidate) {
            return (Resolve-Path $candidate).Path
        }
    }
    $command = Get-Command iscc.exe -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }
    return $null
}

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
New-Item -ItemType Directory -Path $dist -Force | Out-Null

$docs = @(
    (Join-Path $root 'README.md'),
    (Join-Path $root 'CHANGELOG.md'),
    (Join-Path $root 'LICENSE'),
    (Join-Path $root 'THIRD_PARTY_LICENSES.md')
)

$artifacts = @()

if (-not $SkipPortable) {
    $name = "luban-notepad-$version-windows-$arch-portable"
    $stage = Join-Path $dist $name
    $zip = Join-Path $dist "${name}.zip"

    Write-Host "==> Staging $name"
    if (Test-Path $stage) {
        Remove-Item $stage -Recurse -Force
    }
    New-Item -ItemType Directory -Path $stage -Force | Out-Null
    Copy-Item $binary (Join-Path $stage 'luban-notepad.exe')
    Copy-Item $docs $stage
    # The marker makes the app keep settings and sessions in data/ next to the
    # executable instead of %APPDATA%\luban-notepad.
    New-Item -ItemType File -Path (Join-Path $stage 'luban-notepad.portable') -Force | Out-Null

    Write-Host "==> Creating ${name}.zip"
    if (Test-Path $zip) {
        Remove-Item $zip -Force
    }
    Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
    Write-Checksum $zip
    $artifacts += $zip, "${zip}.sha256"
}

if (-not $SkipInstaller) {
    $iscc = Find-Iscc $InnoSetupPath
    if (-not $iscc) {
        throw 'Inno Setup 6 not found. Install it (winget install JRSoftware.InnoSetup), set INNO_SETUP_ISCC, or pass -SkipInstaller.'
    }

    # Keep the installer sources separate from the portable stage: the marker
    # file must not be installed, otherwise an installed copy would switch to
    # portable configuration under Program Files.
    $installerStage = Join-Path $dist '.installer-stage'
    Write-Host '==> Staging installer files'
    if (Test-Path $installerStage) {
        Remove-Item $installerStage -Recurse -Force
    }
    New-Item -ItemType Directory -Path $installerStage -Force | Out-Null
    Copy-Item $binary (Join-Path $installerStage 'luban-notepad.exe')
    Copy-Item $docs $installerStage
    # Inno Setup only renders license files as Unicode when they start with a
    # UTF-8 BOM; LICENSE is BOM-less UTF-8 with Chinese text.
    $licenseText = [System.IO.File]::ReadAllText((Join-Path $root 'LICENSE'))
    [System.IO.File]::WriteAllText(
        (Join-Path $installerStage 'LICENSE.utf8.txt'),
        $licenseText,
        (New-Object System.Text.UTF8Encoding($true))
    )

    $isccArgs = @(
        "/DAppVersion=$version",
        "/DSourceDir=$installerStage",
        "/DOutputDir=$dist",
        "/DArch=$arch",
        "/DIconFile=$(Join-Path $root 'packaging\windows\luban-notepad.ico')"
    )
    if ($arch -eq 'aarch64') {
        $isccArgs += '/DArm64=1'
    }

    # Use the Simplified Chinese wizard when the compiler ships the unofficial
    # Inno Setup translation; otherwise install in English.
    $isccDir = Split-Path $iscc -Parent
    foreach ($relative in @('Languages\ChineseSimplified.isl', 'Languages\Unofficial\ChineseSimplified.isl')) {
        if (Test-Path (Join-Path $isccDir $relative)) {
            $isccArgs += '/DChineseLanguage=1'
            $isccArgs += "/DChineseMessagesFile=compiler:$relative"
            break
        }
    }

    $iss = Join-Path $root 'packaging\windows\luban-notepad.iss'
    Write-Host "==> Compiling installer with $iscc"
    & $iscc @isccArgs $iss
    if ($LASTEXITCODE -ne 0) {
        throw 'Inno Setup compilation failed'
    }

    $setup = Join-Path $dist "luban-notepad-$version-windows-$arch-setup.exe"
    if (-not (Test-Path $setup)) {
        throw "installer was not created at $setup"
    }
    Write-Checksum $setup
    $artifacts += $setup, "${setup}.sha256"

    Remove-Item $installerStage -Recurse -Force
}

Write-Host ''
Write-Host 'Artifacts:'
if ($artifacts.Count -gt 0) {
    $artifacts | ForEach-Object { Get-Item $_ } | Format-Table Name, Length -AutoSize
}
