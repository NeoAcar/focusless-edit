[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [string]$OutputDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $repositoryRoot "dist\Focusless-Edit-windows-x64"
}
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
if ($OutputDirectory -eq [IO.Path]::GetPathRoot($OutputDirectory) -or $OutputDirectory -eq $repositoryRoot) {
    throw "Refusing to use unsafe package output directory: $OutputDirectory"
}

. (Join-Path $PSScriptRoot "environment.ps1")

Push-Location $repositoryRoot
try {
    if (-not $SkipBuild) {
        & cargo build --workspace --release --locked
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed." }
    }

    $executable = Join-Path $repositoryRoot "target\release\focusless-edit.exe"
    if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
        throw "Release executable was not found at $executable"
    }

    if (Test-Path -LiteralPath $OutputDirectory) {
        Remove-Item -LiteralPath $OutputDirectory -Recurse -Force
    }
    New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null

    Copy-Item -LiteralPath $executable -Destination $OutputDirectory
    Get-ChildItem -LiteralPath (Join-Path $env:FOCUSLESS_VIPS_DIR "bin") -Filter "*.dll" |
        Copy-Item -Destination $OutputDirectory
    Get-ChildItem -LiteralPath (Join-Path $env:FOCUSLESS_VIPS_DIR "bin") -Directory `
        -Filter "vips-modules-*" |
        Copy-Item -Destination $OutputDirectory -Recurse

    $licensesDirectory = Join-Path $OutputDirectory "THIRD-PARTY-LICENSES"
    New-Item -ItemType Directory -Path $licensesDirectory -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $env:FOCUSLESS_VIPS_DIR "LICENSE") `
        -Destination (Join-Path $licensesDirectory "libvips-LGPL-2.1.txt")
    Copy-Item -LiteralPath (Join-Path $repositoryRoot "docs\windows.md") `
        -Destination (Join-Path $OutputDirectory "WINDOWS-README.md")

    $archivePath = "$OutputDirectory.zip"
    if (Test-Path -LiteralPath $archivePath) {
        Remove-Item -LiteralPath $archivePath -Force
    }
    Compress-Archive -Path (Join-Path $OutputDirectory "*") -DestinationPath $archivePath
    Write-Host "Portable Windows package created at $archivePath"
}
finally {
    Pop-Location
}
