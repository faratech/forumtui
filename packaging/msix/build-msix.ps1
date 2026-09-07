<#
.SYNOPSIS
  Cross-build wftui for x64/x86/arm64, pack each as MSIX, and sign the
  packages with the WindowsForum Azure Trusted Signing kit.

.DESCRIPTION
  Windows-only — needs the Rust MSVC toolchain, the Windows SDK (for
  makeappx.exe; the sign kit locates signtool.exe itself), and the signing
  kit at -SignKitRoot (defaults to C:\code\sign, i.e. what's checked in at
  /root/.sign on the build server — copy that directory here first, with
  .env.codesigning's client secret filled in, before running this).

  Per architecture: `cargo build --release --target <triple>`, stage the
  exe + Assets\, render AppxManifest.template.xml, `makeappx pack`, then
  hand the .msix to the sign kit's sign.ps1 (signs + verifies).

  Nothing here is run from the Linux repo checkout — this script only
  works on the Windows box that owns the signing credentials.

.EXAMPLE
  .\build-msix.ps1
  Builds and signs x64 + x86 + arm64 MSIX packages plus a signed bundle.

.EXAMPLE
  .\build-msix.ps1 -Architectures x64 -SkipSign
  Just the x64 package, unsigned — for a local sideload smoke test.

.EXAMPLE
  .\build-msix.ps1 -SignKitRoot 'C:\code\sign' -OutDir 'C:\code\sign\dist'
#>

[CmdletBinding()]
param(
    [ValidateSet("x64", "x86", "arm64")]
    [string[]]$Architectures = @("x64", "x86", "arm64"),

    # Overrides the version read from wftui/Cargo.toml. Must be a bare
    # x.y.z — the 4th (build) component is always appended as .0.
    [string]$Version,

    [string]$SignKitRoot = "C:\code\sign",

    [string]$OutDir = "C:\code\sign\dist",

    # Skip signtool entirely — produces sideloadable-only (untrusted) packages.
    [switch]$SkipSign,

    # Skip producing the combined .msixbundle (three architectures in one
    # installer; winget and `Add-AppxPackage` both accept it directly).
    [switch]$SkipBundle
)

$ErrorActionPreference = "Stop"

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$PackagingRoot = $PSScriptRoot
$ManifestTemplate = Join-Path $PackagingRoot "AppxManifest.template.xml"
$AssetsDir = Join-Path $PackagingRoot "assets"

$TripleByArch = @{
    "x64"   = "x86_64-pc-windows-msvc"
    "x86"   = "i686-pc-windows-msvc"
    "arm64" = "aarch64-pc-windows-msvc"
}

# ---------------------------------------------------------------------------
# Tooling lookup (mirrors common.ps1's Get-SignTool — makeappx has no
# architecture-pairing requirement, so one copy handles every target).
# ---------------------------------------------------------------------------

function Get-MakeAppx {
    $roots = @(
        "${env:ProgramFiles(x86)}\Windows Kits\10\bin",
        "$env:ProgramFiles\Windows Kits\10\bin"
    ) | Where-Object { $_ -and (Test-Path $_) }

    $candidate = Get-ChildItem -Path $roots -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match '^10\.' } |
        Sort-Object { [version]($_.Name -replace '[^0-9.]', '') } -Descending |
        ForEach-Object { Join-Path $_.FullName "x64\makeappx.exe" } |
        Where-Object { Test-Path $_ } |
        Select-Object -First 1

    if ($candidate) { return $candidate }

    $onPath = Get-Command makeappx.exe -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }

    return $null
}

# ---------------------------------------------------------------------------
# Version
# ---------------------------------------------------------------------------

if (-not $Version) {
    $cargoToml = Get-Content (Join-Path $RepoRoot "wftui\Cargo.toml") -Raw
    if ($cargoToml -match '(?m)^version\s*=\s*"([^"]+)"') {
        $Version = $Matches[1]
    } else {
        throw "Could not read version from wftui\Cargo.toml; pass -Version explicitly."
    }
}
$MsixVersion = "$Version.0"
Write-Host "wftui version: $Version (MSIX Identity Version: $MsixVersion)" -ForegroundColor Cyan

# ---------------------------------------------------------------------------
# Prerequisites
# ---------------------------------------------------------------------------

$makeappx = Get-MakeAppx
if (-not $makeappx) { throw "makeappx.exe not found. Install the Windows SDK (Signing Tools for Desktop Apps / MSIX Packaging Tool component)." }
Write-Verbose "makeappx: $makeappx"

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "cargo not found. Install Rust (rustup) first."
}

if (-not $SkipSign) {
    $signScript = Join-Path $SignKitRoot "sign.ps1"
    if (-not (Test-Path $signScript)) {
        throw "Signing kit not found at $SignKitRoot (expected sign.ps1). Copy the kit from /root/.sign, or pass -SkipSign for an unsigned build."
    }
}

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$Work = Join-Path $env:TEMP "wftui-msix-build-$Version"
if (Test-Path $Work) { Remove-Item -Recurse -Force $Work }
New-Item -ItemType Directory -Force -Path $Work | Out-Null

$produced = @()

# ---------------------------------------------------------------------------
# Per-architecture build + pack + sign
# ---------------------------------------------------------------------------

foreach ($arch in $Architectures) {
    $triple = $TripleByArch[$arch]
    Write-Host "`n=== $arch ($triple) ===" -ForegroundColor Cyan

    Write-Host "-- rustup target add $triple"
    rustup target add $triple | Out-Null

    Write-Host "-- cargo build --release --target $triple -p wftui"
    Push-Location $RepoRoot
    try {
        cargo build --release --target $triple -p wftui
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed for $triple" }
    } finally {
        Pop-Location
    }

    $exe = Join-Path $RepoRoot "target\$triple\release\wftui.exe"
    if (-not (Test-Path $exe)) { throw "Expected build output missing: $exe" }

    $stage = Join-Path $Work $arch
    New-Item -ItemType Directory -Force -Path $stage | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $stage "Assets") | Out-Null

    Copy-Item $exe (Join-Path $stage "wftui.exe") -Force
    Copy-Item (Join-Path $AssetsDir "*.png") (Join-Path $stage "Assets") -Force

    $manifest = Get-Content $ManifestTemplate -Raw
    $manifest = $manifest.Replace("{{VERSION}}", $MsixVersion).Replace("{{ARCH}}", $arch)
    Set-Content -Path (Join-Path $stage "AppxManifest.xml") -Value $manifest -NoNewline

    $msixPath = Join-Path $OutDir "wftui-$Version-$arch.msix"
    if (Test-Path $msixPath) { Remove-Item $msixPath -Force }

    Write-Host "-- makeappx pack ($arch)"
    & $makeappx pack /d $stage /p $msixPath /o
    if ($LASTEXITCODE -ne 0) { throw "makeappx pack failed for $arch" }

    if (-not $SkipSign) {
        Write-Host "-- signing ($arch)"
        & (Join-Path $SignKitRoot "sign.ps1") $msixPath -Description "WindowsForum TUI" -DescriptionUrl "https://windowsforum.com"
        if ($LASTEXITCODE -ne 0) { throw "Signing failed for $arch" }
    } else {
        Write-Host "-- skipped signing ($arch)" -ForegroundColor Yellow
    }

    $produced += $msixPath
    Write-Host "[OK] $msixPath" -ForegroundColor Green
}

# ---------------------------------------------------------------------------
# Bundle (all architectures, one installer)
# ---------------------------------------------------------------------------

if (-not $SkipBundle -and $produced.Count -gt 1) {
    Write-Host "`n=== bundle ===" -ForegroundColor Cyan
    $bundleDir = Join-Path $Work "bundle"
    New-Item -ItemType Directory -Force -Path $bundleDir | Out-Null
    foreach ($p in $produced) { Copy-Item $p $bundleDir -Force }

    $bundlePath = Join-Path $OutDir "wftui-$Version.msixbundle"
    if (Test-Path $bundlePath) { Remove-Item $bundlePath -Force }

    & $makeappx bundle /d $bundleDir /p $bundlePath /o
    if ($LASTEXITCODE -ne 0) { throw "makeappx bundle failed" }

    if (-not $SkipSign) {
        Write-Host "-- signing bundle"
        & (Join-Path $SignKitRoot "sign.ps1") $bundlePath -Description "WindowsForum TUI" -DescriptionUrl "https://windowsforum.com"
        if ($LASTEXITCODE -ne 0) { throw "Signing failed for bundle" }
    }

    $produced += $bundlePath
    Write-Host "[OK] $bundlePath" -ForegroundColor Green
}

# ---------------------------------------------------------------------------
# Checksums
# ---------------------------------------------------------------------------

$shaPath = Join-Path $OutDir "wftui-$Version.SHA256SUMS.txt"
$lines = foreach ($p in $produced) {
    $hash = (Get-FileHash $p -Algorithm SHA256).Hash.ToLower()
    "$hash  $(Split-Path $p -Leaf)"
}
Set-Content -Path $shaPath -Value $lines
Write-Host "`nWrote checksums: $shaPath" -ForegroundColor Cyan

Write-Host "`nDone. $($produced.Count) package(s) in $OutDir" -ForegroundColor Green
