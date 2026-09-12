<#
.SYNOPSIS
  Shared helpers for the Trusted Signing scripts (dot-sourced, not run directly).

.DESCRIPTION
  Resolves signtool.exe, the Azure.CodeSigning dlib, metadata.json, and loads
  auth credentials from .env.codesigning if they aren't already in the
  environment.
#>

$SignKitRoot = $PSScriptRoot

# ---------------------------------------------------------------------------
# Credentials
# ---------------------------------------------------------------------------

function Import-SigningEnv {
    <#
      Loads KEY=VALUE lines from .env.codesigning into the current process
      environment. Existing environment variables win, so CI secret stores and
      an already-configured shell are never overwritten by the local file.
    #>
    param([string]$EnvFile = (Join-Path $SignKitRoot ".env.codesigning"))

    if (-not (Test-Path $EnvFile)) { return }

    foreach ($line in Get-Content $EnvFile) {
        $trimmed = $line.Trim()
        if (-not $trimmed -or $trimmed.StartsWith("#")) { continue }

        $split = $trimmed.IndexOf("=")
        if ($split -lt 1) { continue }

        $key = $trimmed.Substring(0, $split).Trim()
        $value = $trimmed.Substring($split + 1).Trim().Trim('"')

        if (-not [Environment]::GetEnvironmentVariable($key, "Process")) {
            [Environment]::SetEnvironmentVariable($key, $value, "Process")
        }
    }
}

function Test-SigningCredentials {
    <# Returns the name of the first missing auth variable, or $null if all set. #>
    foreach ($name in "AZURE_TENANT_ID", "AZURE_CLIENT_ID", "AZURE_CLIENT_SECRET") {
        $value = [Environment]::GetEnvironmentVariable($name, "Process")
        if (-not $value -or $value -like "<*>") { return $name }
    }
    return $null
}

# ---------------------------------------------------------------------------
# Tooling
# ---------------------------------------------------------------------------

# The dlib ships x64 and x86 only, and its architecture must match signtool's.
# On ARM64 hosts the x64 pair runs fine under emulation.
function Get-SigningArch {
    if ($env:PROCESSOR_ARCHITECTURE -eq "x86" -and -not $env:PROCESSOR_ARCHITEW6432) { return "x86" }
    return "x64"
}

function Get-SignTool {
    <# Newest Windows SDK signtool.exe for the chosen architecture. #>
    param([string]$Arch = (Get-SigningArch))

    $roots = @(
        "${env:ProgramFiles(x86)}\Windows Kits\10\bin",
        "$env:ProgramFiles\Windows Kits\10\bin"
    ) | Where-Object { $_ -and (Test-Path $_) }

    $candidate = Get-ChildItem -Path $roots -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match '^10\.' } |
        Sort-Object { [version]($_.Name -replace '[^0-9.]', '') } -Descending |
        ForEach-Object { Join-Path $_.FullName "$Arch\signtool.exe" } |
        Where-Object { Test-Path $_ } |
        Select-Object -First 1

    if ($candidate) { return $candidate }

    # Fall back to whatever is on PATH (e.g. a CI image that ships its own).
    $onPath = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }

    return $null
}

function Get-SigningDlib {
    param([string]$Arch = (Get-SigningArch))
    $dlib = Join-Path $SignKitRoot "lib\$Arch\Azure.CodeSigning.Dlib.dll"
    if (Test-Path $dlib) { return $dlib }
    return $null
}

function Get-SigningMetadata {
    $metadata = Join-Path $SignKitRoot "metadata.json"
    if (Test-Path $metadata) { return $metadata }
    return $null
}

# Timestamp authority used for every signature. A signature without a valid
# timestamp stops being trusted the moment the certificate expires.
$SigningTimestampUrl = "http://timestamp.acs.microsoft.com"
