[CmdletBinding()]
param(
    [string]$LibvipsDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($LibvipsDirectory)) {
    $LibvipsDirectory = & (Join-Path $PSScriptRoot "bootstrap-libvips.ps1")
}
$LibvipsDirectory = [IO.Path]::GetFullPath($LibvipsDirectory)

function Add-EnvironmentPath {
    param(
        [string]$Name,
        [string]$Entry
    )

    $current = [Environment]::GetEnvironmentVariable($Name, "Process")
    $entries = if ([string]::IsNullOrWhiteSpace($current)) {
        @()
    }
    else {
        $current -split [IO.Path]::PathSeparator
    }
    if ($entries -notcontains $Entry) {
        [Environment]::SetEnvironmentVariable(
            $Name,
            ((@($Entry) + $entries) -join [IO.Path]::PathSeparator),
            "Process"
        )
    }
}

$env:FOCUSLESS_VIPS_DIR = $LibvipsDirectory
Add-EnvironmentPath -Name "PATH" -Entry (Join-Path $LibvipsDirectory "bin")
Add-EnvironmentPath -Name "LIB" -Entry (Join-Path $LibvipsDirectory "lib")

Write-Host "Focusless Edit Windows environment is ready."
