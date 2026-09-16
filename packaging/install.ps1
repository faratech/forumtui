# install.ps1 — install Forum Terminal (TUI) (wftui) from a GitHub release.
#
#   irm https://raw.githubusercontent.com/faratech/forumtui/main/packaging/install.ps1 | iex
#   .\install.ps1                          # latest release, per-user install
#   .\install.ps1 -Edition xf              # Terminal for XenForo
#   .\install.ps1 -InstallDir D:\tools     # anywhere you like
#
# Detects the CPU (x64 / x86 / arm64), downloads the release's bare,
# dual-signed wftui-<version>-windows-<arch>.exe and SHA256SUMS.txt, verifies
# the digest the release published, and installs it. The signed MSIX is the
# recommended install on Windows — this script is the "just give me the exe"
# path (a portable copy, or a machine where the Store-style install is not
# wanted).
#
# Linux and macOS: use packaging/install.sh. macOS is not built yet.
[CmdletBinding()]
param(
    # "" is Forum Terminal (TUI) (the built-in site); "xf" is Terminal for
    # XenForo — the same suffix the release assets and the self-updater use
    # to keep the two editions' binaries apart.
    [ValidateSet('', 'xf')]
    [string]$Edition = '',

    # Where the exe goes. Per-user by default; a system-wide dir needs an
    # elevated shell, same as anything else under $env:ProgramFiles.
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'wftui')
)

$ErrorActionPreference = 'Stop'
# Older Windows PowerShell defaults to TLS versions this endpoint refuses.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$Repo = 'faratech/forumtui'
$Feed = $env:WFTUI_UPDATE_URL
if (-not $Feed) { $Feed = "https://api.github.com/repos/$Repo/releases/latest" }

# ---- what are we running on ------------------------------------------------
# PROCESSOR_ARCHITECTURE is what the MSIX packaging and the native console
# probe both key off; PROCESSOR_ARCHITEW6432 catches a 32-bit PowerShell on
# 64-bit Windows.
$archEnv = $env:PROCESSOR_ARCHITEW6432
if (-not $archEnv) { $archEnv = $env:PROCESSOR_ARCHITECTURE }
switch ($archEnv) {
    'AMD64' { $Arch = 'x64' }
    'x86'   { $Arch = 'x86' }
    'ARM64' { $Arch = 'arm64' }
    default { throw "unsupported CPU architecture: $archEnv (releases carry windows-x64, windows-x86, windows-arm64)" }
}

# ---- pick the release ------------------------------------------------------
try {
    $release = Invoke-RestMethod -Uri $Feed -UseBasicParsing
} catch {
    throw "cannot reach the release feed ($Feed). If it answered 404: no release has been published yet. Once one ships, this script finds it."
}
if (-not $release.tag_name) { throw "could not read tag_name from the release feed" }
$tag = $release.tag_name

$suffix = if ($Edition) { "-$Edition" } else { '' }
$asset = "wftui$suffix-$($tag.TrimStart('v'))-windows-$Arch.exe"
$sumsAsset = 'SHA256SUMS.txt'

$download = $release.assets | Where-Object { $_.name -eq $asset }
if (-not $download) {
    throw "release $tag has no asset named $asset (assets: $(($release.assets | ForEach-Object name) -join ', '))"
}
$sums = $release.assets | Where-Object { $_.name -eq $sumsAsset }
if (-not $sums) { throw "release $tag has no $sumsAsset" }

# ---- download and verify ----------------------------------------------------
$tmp = Join-Path ([IO.Path]::GetTempPath()) "wftui-install-$PID"
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
try {
    Write-Host "fetching $asset ..."
    $exePath = Join-Path $tmp $asset
    Invoke-WebRequest -Uri $download.browser_download_url -OutFile $exePath -UseBasicParsing
    $sumsPath = Join-Path $tmp $sumsAsset
    Invoke-WebRequest -Uri $sums.browser_download_url -OutFile $sumsPath -UseBasicParsing

    # `sha256sum` format: "<digest>  <name>", two spaces — the same file the
    # client's self-updater verifies against.
    $want = (Get-Content $sumsPath |
        Where-Object { $_ -match "^([0-9a-fA-F]{64})  $([regex]::Escape($asset))$" } |
        ForEach-Object { $Matches[1] } | Select-Object -First 1)
    if (-not $want) { throw "$sumsAsset has no line for $asset" }

    $got = (Get-FileHash -Algorithm SHA256 -Path $exePath).Hash.ToLower()
    if ($got -ne $want.ToLower()) { throw "sha256 mismatch for ${asset}: got $got, release says $want" }
    Write-Host "verified  $got"

    # ---- install ------------------------------------------------------------
    # Install-then-rename: a running wftui keeps its old inode (Windows
    # refuses to overwrite a running image outright; its own self-update
    # renames it aside first). Same contract, one level down.
    if (-not (Test-Path $InstallDir)) { New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null }
    $target = Join-Path $InstallDir 'wftui.exe'
    $staged = Join-Path $InstallDir '.wftui.new'
    if (Test-Path $target) {
        $old = Join-Path $InstallDir 'wftui.exe.old'
        if (Test-Path $old) { Remove-Item $old -Force }
        Move-Item $target $old -Force
    }
    Move-Item $exePath $staged -Force
    try {
        Move-Item $staged $target -Force
    } catch {
        # The old image was running; put it back rather than leave nothing.
        if (Test-Path (Join-Path $InstallDir 'wftui.exe.old')) {
            Move-Item (Join-Path $InstallDir 'wftui.exe.old') $target -Force
        }
        throw "could not move wftui.exe into place (was it running?): $($_.Exception.Message)"
    }
    if (Test-Path (Join-Path $InstallDir 'wftui.exe.old')) {
        try { Remove-Item (Join-Path $InstallDir 'wftui.exe.old') -Force } catch { }
    }

    Write-Host "installed $target ($tag)"
    if ($env:PATH -notlike "*$InstallDir*") {
        Write-Host "note: $InstallDir is not in your PATH"
    }
    Write-Host "run it:   wftui"
} finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
