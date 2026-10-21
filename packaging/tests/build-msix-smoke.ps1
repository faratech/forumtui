# Verify packager output paths with a controlled compiler and the real SDK packer.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$FixtureExe,
    [ValidateSet('x64', 'x86', 'arm64')]
    [string]$Architecture = 'x64'
)

$ErrorActionPreference = 'Stop'
$FixtureExe = (Resolve-Path $FixtureExe).Path
$packaging = (Resolve-Path (Join-Path $PSScriptRoot '..\msix')).Path
$triple = switch ($Architecture) {
    'x64' { 'x86_64-pc-windows-msvc' }
    'x86' { 'i686-pc-windows-msvc' }
    'arm64' { 'aarch64-pc-windows-msvc' }
}
$work = Join-Path ([IO.Path]::GetTempPath()) ('wftui-packaging-smoke-' + [guid]::NewGuid().ToString('N'))
$previousTarget = [Environment]::GetEnvironmentVariable('CARGO_TARGET_DIR', 'Process')
$previousTemp = [Environment]::GetEnvironmentVariable('TEMP', 'Process')
$previousTmp = [Environment]::GetEnvironmentVariable('TMP', 'Process')
try {
    New-Item -ItemType Directory -Path $work | Out-Null
    $temporary = Join-Path $work 'temporary'
    New-Item -ItemType Directory -Path $temporary | Out-Null
    $env:TEMP = $temporary
    $env:TMP = $temporary
    foreach ($case in 'DefaultDirectory', 'CustomAbsoluteDirectory', 'CustomRelativeDirectory', 'DriveRelativeDirectory') {
        $fixture = Join-Path $work $case
        $scripts = Join-Path $fixture 'packaging\msix'
        New-Item -ItemType Directory -Path $scripts -Force | Out-Null
        Copy-Item (Join-Path $packaging 'build-msix.ps1') $scripts
        Copy-Item (Join-Path $packaging 'AppxManifest.template.xml') $scripts
        Copy-Item (Join-Path $packaging 'assets') $scripts -Recurse
        New-Item -ItemType Directory -Path (Join-Path $fixture 'wftui') | Out-Null
        Set-Content (Join-Path $fixture 'wftui\Cargo.toml') 'version = "9.8.7"'
        $stale = Join-Path $fixture "target\$triple\release\wftui.exe"
        New-Item -ItemType Directory -Path (Split-Path $stale) -Force | Out-Null
        Copy-Item $FixtureExe $stale
        [IO.File]::AppendAllText($stale, 'STALE', [Text.Encoding]::ASCII)
        switch ($case) {
            'DefaultDirectory' { Remove-Item Env:\CARGO_TARGET_DIR -ErrorAction SilentlyContinue }
            'CustomAbsoluteDirectory' { $env:CARGO_TARGET_DIR = Join-Path $fixture 'custom output' }
            'CustomRelativeDirectory' { $env:CARGO_TARGET_DIR = 'relative output' }
            'DriveRelativeDirectory' {
                $drive = Split-Path -Path $fixture -Qualifier
                if ($drive -notmatch '^[A-Za-z]:$') { throw 'The drive-relative check needs a drive-letter temporary directory.' }
                $env:CARGO_TARGET_DIR = $drive + 'drive-relative output'
            }
        }

        $mocks = New-Module -Name ('WftuiPackagingSmoke' + [guid]::NewGuid().ToString('N')) -ArgumentList $fixture, $FixtureExe -ScriptBlock {
            param($FixtureRoot, $FixtureExecutable)
            function rustup { $global:LASTEXITCODE = 0 }
            function cargo {
                $arguments = @($args)
                $targetAt = [Array]::IndexOf($arguments, '--target')
                if ($targetAt -lt 0) { throw 'The packager did not select an architecture.' }
                $target = $arguments[$targetAt + 1]
                $directoryAt = [Array]::IndexOf($arguments, '--target-dir')
                $directory = if ($directoryAt -ge 0) {
                    $arguments[$directoryAt + 1]
                } elseif ($env:CARGO_TARGET_DIR) {
                    $env:CARGO_TARGET_DIR
                } else {
                    Join-Path $script:FixtureRoot 'ambient-config-target'
                }
                if (-not [IO.Path]::IsPathRooted($directory)) {
                    $directory = Join-Path $script:FixtureRoot $directory
                }
                $output = Join-Path $directory "$target\release\wftui.exe"
                New-Item -ItemType Directory -Path (Split-Path $output) -Force | Out-Null
                Copy-Item $script:FixtureExecutable $output -Force
                $edition = if ($arguments -contains '--no-default-features') { 'GENERIC' } else { 'BUILTIN' }
                [IO.File]::AppendAllText($output, "FRESH-$edition", [Text.Encoding]::ASCII)
                $global:LASTEXITCODE = 0
            }
            Export-ModuleMember -Function cargo, rustup
        }
        try {
            Import-Module $mocks -Scope Local
            $output = Join-Path $fixture 'dist'
            & (Join-Path $scripts 'build-msix.ps1') -Architectures $Architecture -SkipSign -SkipBundle -OutDir $output
            foreach ($edition in 'BUILTIN', 'GENERIC') {
                $prefix = if ($edition -eq 'GENERIC') { 'wftui-xf' } else { 'wftui' }
                $asset = Join-Path $output "$prefix-9.8.7-windows-$Architecture.exe"
                $bytes = [IO.File]::ReadAllBytes($asset)
                $tail = [Text.Encoding]::ASCII.GetString($bytes, [Math]::Max(0, $bytes.Length - 32), [Math]::Min(32, $bytes.Length))
                if (-not $tail.EndsWith("FRESH-$edition")) {
                    throw "${case}: packaged a stale or wrong-edition binary for $edition."
                }
            }
            Write-Host "PASS $case ($Architecture, both editions)"
        } finally {
            Remove-Module $mocks -ErrorAction SilentlyContinue
        }
    }
} finally {
    [Environment]::SetEnvironmentVariable('CARGO_TARGET_DIR', $previousTarget, 'Process')
    [Environment]::SetEnvironmentVariable('TEMP', $previousTemp, 'Process')
    [Environment]::SetEnvironmentVariable('TMP', $previousTmp, 'Process')
    if (Test-Path $work) { Remove-Item $work -Recurse -Force }
}
