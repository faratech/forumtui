<#
.SYNOPSIS
  Dual-sign EXE/DLL files: Fara Technologies LLC first, Mike Fara second.
  Other formats retain the existing MikeFara package/script identity.

.DESCRIPTION
  Locates signtool.exe and the Azure.CodeSigning dlib automatically, loads
  credentials from .env.codesigning (or the existing environment / CI secret
  store), signs every file given, and verifies the result.

.EXAMPLE
  .\sign.ps1 .\MyApp.exe

.EXAMPLE
  .\sign.ps1 .\dist\*.exe .\dist\*.msi -Description "WFDiag" -DescriptionUrl "https://windowsforum.com"

.EXAMPLE
  Get-ChildItem .\dist -Filter *.exe -Recurse | .\sign.ps1
#>

[CmdletBinding()]
param(
    # Files to sign. Wildcards are expanded; also accepts pipeline input.
    [Parameter(Mandatory = $true, Position = 0, ValueFromPipeline = $true, ValueFromPipelineByPropertyName = $true)]
    [Alias("FullName")]
    [string[]]$Path,

    # Optional publisher description embedded in the signature (shown in some UAC prompts).
    [string]$Description,

    # Optional URL embedded alongside the description.
    [string]$DescriptionUrl,

    # Skip the post-sign `signtool verify` pass.
    [switch]$SkipVerify
)

begin {
    $ErrorActionPreference = "Stop"
    . (Join-Path $PSScriptRoot "common.ps1")

    Import-SigningEnv
    $missing = Test-SigningCredentials
    if ($missing) {
        throw "$missing is not set. Copy .env.codesigning.example to .env.codesigning and fill in the client secret, or set the variable in your shell / CI secret store."
    }

    $arch = Get-SigningArch
    $signtool = Get-SignTool -Arch $arch
    if (-not $signtool) { throw "signtool.exe not found. Install the Windows SDK (Signing Tools for Desktop Apps)." }

    $dlib = Get-SigningDlib -Arch $arch
    if (-not $dlib) { throw "lib\$arch\Azure.CodeSigning.Dlib.dll is missing. Run .\install-dlib.ps1 to fetch it." }

    $metadata = Get-SigningMetadata
    if (-not $metadata) { throw "metadata.json not found next to sign.ps1." }

    Write-Verbose "signtool: $signtool"
    Write-Verbose "dlib:     $dlib"

    $failed = @()
    $signed = @()
}

process {
    foreach ($item in $Path) {
        $resolved = Resolve-Path -Path $item -ErrorAction SilentlyContinue
        if (-not $resolved) {
            Write-Host "[SKIP] No such file: $item" -ForegroundColor Yellow
            $failed += $item
            continue
        }

        foreach ($file in $resolved) {
            if (Test-Path $file -PathType Container) { continue }

            $dual = [IO.Path]::GetExtension($file.Path) -in '.exe', '.dll'
            $primaryMetadata = $metadata
            if ($dual) { $primaryMetadata = Join-Path $PSScriptRoot 'metadata-business.json' }
            $args = @(
                "sign", "/v",
                "/fd", "SHA256",
                "/tr", $SigningTimestampUrl, "/td", "SHA256",
                "/dlib", $dlib,
                "/dmdf", $primaryMetadata
            )
            if ($Description)    { $args += @("/d", $Description) }
            if ($DescriptionUrl) { $args += @("/du", $DescriptionUrl) }
            $args += $file.Path

            & $signtool @args
            if ($LASTEXITCODE -ne 0) {
                Write-Host "[FAIL] signtool exit code $LASTEXITCODE for $($file.Path)" -ForegroundColor Red
                $failed += $file.Path
                continue
            }

            if ($dual) {
                $secondArgs = @('sign', '/v', '/as', '/fd', 'SHA256',
                    '/tr', $SigningTimestampUrl, '/td', 'SHA256',
                    '/dlib', $dlib, '/dmdf', $metadata)
                if ($Description) { $secondArgs += @('/d', $Description) }
                if ($DescriptionUrl) { $secondArgs += @('/du', $DescriptionUrl) }
                $secondArgs += $file.Path
                & $signtool @secondArgs
                if ($LASTEXITCODE -ne 0) {
                    Write-Host "[FAIL] Second signature failed: $($file.Path)"
                    $failed += $file.Path
                    continue
                }
            }

            if (-not $SkipVerify) {
                & $signtool verify /pa /all /v $file.Path
                if ($LASTEXITCODE -ne 0) {
                    Write-Host "[FAIL] Signature verification failed for $($file.Path)" -ForegroundColor Red
                    $failed += $file.Path
                    continue
                }
            }

            Write-Host "[OK] Signed: $($file.Path)" -ForegroundColor Green
            $signed += $file.Path
        }
    }
}

end {
    Write-Host ""
    Write-Host "Signed $($signed.Count) file(s); $($failed.Count) failure(s)." -ForegroundColor Cyan
    if ($failed.Count -gt 0) { exit 1 }
}
