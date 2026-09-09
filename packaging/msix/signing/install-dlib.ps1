<#
.SYNOPSIS
  Download the Microsoft.Trusted.Signing.Client package and install its dlib
  into .\lib\x64 and .\lib\x86.

.DESCRIPTION
  Run this once per machine (already done on this one). No nuget.exe or dotnet
  SDK required — it pulls the .nupkg straight from nuget.org and unzips it.

.EXAMPLE
  .\install-dlib.ps1
#>

[CmdletBinding()]
param(
    [string]$Version = "1.0.60",
    [switch]$Force
)

$ErrorActionPreference = "Stop"
$libRoot = Join-Path $PSScriptRoot "lib"

if ((Test-Path (Join-Path $libRoot "x64\Azure.CodeSigning.Dlib.dll")) -and -not $Force) {
    Write-Host "dlib already installed in $libRoot (use -Force to reinstall)." -ForegroundColor Yellow
    exit 0
}

$temp = Join-Path ([IO.Path]::GetTempPath()) ("tsc-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force $temp | Out-Null

try {
    $nupkg = Join-Path $temp "package.zip"
    $url = "https://www.nuget.org/api/v2/package/Microsoft.Trusted.Signing.Client/$Version"
    Write-Host "Downloading Microsoft.Trusted.Signing.Client $Version ..."
    Invoke-WebRequest -Uri $url -OutFile $nupkg

    Expand-Archive $nupkg -DestinationPath (Join-Path $temp "pkg") -Force

    New-Item -ItemType Directory -Force $libRoot | Out-Null
    foreach ($arch in "x64", "x86") {
        $src = Join-Path $temp "pkg\bin\$arch"
        if (-not (Test-Path $src)) {
            Write-Host "[WARN] Package has no bin\$arch folder; skipping." -ForegroundColor Yellow
            continue
        }
        $dest = Join-Path $libRoot $arch
        if (Test-Path $dest) { Remove-Item $dest -Recurse -Force }
        Copy-Item $src $dest -Recurse -Force
        Write-Host "[OK] Installed $arch dlib to $dest" -ForegroundColor Green
    }

    Set-Content -Path (Join-Path $libRoot "VERSION.txt") -Value "Microsoft.Trusted.Signing.Client $Version"
    Write-Host ""
    Write-Host "Done. Next: .\test-signing.ps1" -ForegroundColor Cyan
}
finally {
    Remove-Item $temp -Recurse -Force -ErrorAction SilentlyContinue
}
