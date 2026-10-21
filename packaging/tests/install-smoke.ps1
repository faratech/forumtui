# Native Windows regression checks for install.ps1 (#105).
# Downloads are replaced with deterministic fixtures; every file is disposable.
$ErrorActionPreference = 'Stop'

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'Run this smoke test on Windows to exercise its file-sharing locks.'
}

$installer = Join-Path $PSScriptRoot '..\install.ps1'
$archEnv = $env:PROCESSOR_ARCHITEW6432
if (-not $archEnv) { $archEnv = $env:PROCESSOR_ARCHITECTURE }
$arch = switch ($archEnv) {
    'AMD64' { 'x64' }
    'x86' { 'x86' }
    'ARM64' { 'arm64' }
    default { throw "Unsupported test architecture: $archEnv" }
}

$asset = "wftui-0.0.4-windows-$arch.exe"
$payload = [Text.Encoding]::ASCII.GetBytes('disposable installer smoke payload')
$hasher = [Security.Cryptography.SHA256]::Create()
try {
    $digest = ([BitConverter]::ToString($hasher.ComputeHash($payload))).Replace('-', '').ToLower()
} finally {
    $hasher.Dispose()
}
$feed = 'https://fixture.invalid/releases/latest'
$release = [pscustomobject]@{
    tag_name = 'v0.0.4'
    assets = @(
        [pscustomobject]@{ name = $asset; browser_download_url = 'https://fixture.invalid/binary' },
        [pscustomobject]@{ name = 'SHA256SUMS.txt'; browser_download_url = 'https://fixture.invalid/sums' }
    )
}

# A private module keeps fixture variables bound to the mock functions when
# the installer invokes them from its own script scope.
$mocks = New-Module -Name ('WftuiInstallerSmoke' + [guid]::NewGuid().ToString('N')) -ArgumentList $release, $payload, "$digest  $asset", $feed -ScriptBlock {
    param($FixtureRelease, $FixturePayload, $FixtureSums, $FixtureFeed)

    function Invoke-RestMethod {
        param([string]$Uri, [switch]$UseBasicParsing)
        if ($Uri -ne $script:FixtureFeed) { throw "Unexpected release URL: $Uri" }
        $script:FixtureRelease
    }

    function Invoke-WebRequest {
        param([string]$Uri, [string]$OutFile, [switch]$UseBasicParsing)
        switch ($Uri) {
            'https://fixture.invalid/binary' {
                [IO.File]::WriteAllBytes($OutFile, $script:FixturePayload)
            }
            'https://fixture.invalid/sums' {
                [IO.File]::WriteAllText($OutFile, $script:FixtureSums, [Text.Encoding]::ASCII)
            }
            default { throw "Unexpected asset URL: $Uri" }
        }
    }

    Export-ModuleMember -Function Invoke-RestMethod, Invoke-WebRequest
}

$work = Join-Path ([IO.Path]::GetTempPath()) ('wftui-installer-smoke-' + [guid]::NewGuid().ToString('N'))
$previousFeed = [Environment]::GetEnvironmentVariable('WFTUI_UPDATE_URL', 'Process')
$previousTemp = [Environment]::GetEnvironmentVariable('TEMP', 'Process')
$previousTmp = [Environment]::GetEnvironmentVariable('TMP', 'Process')
try {
    Import-Module $mocks -Scope Local
    $env:WFTUI_UPDATE_URL = $feed
    New-Item -ItemType Directory -Path $work | Out-Null
    $downloads = Join-Path $work 'downloads'
    New-Item -ItemType Directory -Path $downloads | Out-Null
    $env:TEMP = $downloads
    $env:TMP = $downloads

    $normalDir = Join-Path $work 'normal'
    New-Item -ItemType Directory -Path $normalDir | Out-Null
    $target = Join-Path $normalDir 'wftui.exe'
    [IO.File]::WriteAllText($target, 'old executable')
    & $installer -InstallDir $normalDir
    if ((Get-FileHash $target -Algorithm SHA256).Hash.ToLower() -ne $digest) {
        throw 'SuccessfulReplacement: the installer did not install the verified payload.'
    }
    Write-Host 'PASS SuccessfulReplacement'

    $lockedDir = Join-Path $work 'locked'
    New-Item -ItemType Directory -Path $lockedDir | Out-Null
    $target = Join-Path $lockedDir 'wftui.exe'
    [IO.File]::WriteAllText($target, 'old executable')
    $lock = [IO.File]::Open(
        (Join-Path $lockedDir '.wftui.new'),
        [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None
    )
    $failure = $null
    try {
        try { & $installer -InstallDir $lockedDir } catch { $failure = $_ }
    } finally {
        $lock.Dispose()
    }
    if (-not $failure) {
        throw 'LockedStagePreservesInstalledExecutable: the locked staging file unexpectedly succeeded.'
    }
    if (-not (Test-Path $target) -or [IO.File]::ReadAllText($target) -ne 'old executable') {
        throw 'LockedStagePreservesInstalledExecutable: staging failure removed or changed the installed executable.'
    }
    Write-Host 'PASS LockedStagePreservesInstalledExecutable'
} finally {
    Remove-Module $mocks -ErrorAction SilentlyContinue
    [Environment]::SetEnvironmentVariable('TEMP', $previousTemp, 'Process')
    [Environment]::SetEnvironmentVariable('TMP', $previousTmp, 'Process')
    if ($null -eq $previousFeed) {
        Remove-Item Env:\WFTUI_UPDATE_URL -ErrorAction SilentlyContinue
    } else {
        $env:WFTUI_UPDATE_URL = $previousFeed
    }
    if (Test-Path $work) { Remove-Item $work -Recurse -Force }
}
