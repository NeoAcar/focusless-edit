[CmdletBinding()]
param(
    [string]$Destination
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$libvipsVersion = "8.18.2"
$archiveSha256 = "aec9b8d5e79c06aade9fd51224570c87687675896d4da335955047c211d40e01"
$archiveUrl = "https://github.com/libvips/build-win64-mxe/releases/download/v$libvipsVersion/vips-dev-x64-all-$libvipsVersion.zip"
$archiveRootName = "vips-dev-8.18"
$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path

if ([string]::IsNullOrWhiteSpace($Destination)) {
    $Destination = Join-Path $repositoryRoot ".local\windows\libvips"
}
$Destination = [IO.Path]::GetFullPath($Destination)

function Assert-SafeDestination {
    param([string]$Path)

    $root = [IO.Path]::GetPathRoot($Path)
    if ([string]::IsNullOrWhiteSpace($Path) -or $Path -eq $root -or $Path -eq $repositoryRoot) {
        throw "Refusing to use unsafe libvips destination: $Path"
    }
}

function Test-CompleteInstallation {
    param([string]$Path)

    $marker = Join-Path $Path ".focusless-libvips-version"
    if (-not (Test-Path -LiteralPath $marker -PathType Leaf)) {
        return $false
    }
    $installedVersion = (Get-Content -LiteralPath $marker -Raw).Trim()
    return $installedVersion -eq $libvipsVersion `
        -and (Test-Path -LiteralPath (Join-Path $Path "bin\libvips-42.dll") -PathType Leaf) `
        -and (Test-Path -LiteralPath (Join-Path $Path "lib\libvips.lib") -PathType Leaf)
}

Assert-SafeDestination -Path $Destination
if (Test-CompleteInstallation -Path $Destination) {
    Write-Host "libvips $libvipsVersion is already available at $Destination"
    Write-Output $Destination
    return
}

$temporaryRoot = Join-Path ([IO.Path]::GetTempPath()) ("focusless-libvips-" + [Guid]::NewGuid())
$archivePath = Join-Path $temporaryRoot "libvips.zip"
$extractPath = Join-Path $temporaryRoot "extract"
$stagingPath = "$Destination.staging-$([Guid]::NewGuid())"
$backupPath = "$Destination.backup-$([Guid]::NewGuid())"

try {
    New-Item -ItemType Directory -Path $temporaryRoot, $extractPath -Force | Out-Null
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $previousProgressPreference = $ProgressPreference
    $ProgressPreference = "SilentlyContinue"
    try {
        Write-Host "Downloading the official libvips $libvipsVersion Windows x64 bundle..."
        Invoke-WebRequest -Uri $archiveUrl -OutFile $archivePath -UseBasicParsing
    }
    finally {
        $ProgressPreference = $previousProgressPreference
    }

    $actualSha256 = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualSha256 -ne $archiveSha256) {
        throw "libvips archive checksum mismatch. Expected $archiveSha256, received $actualSha256."
    }

    Expand-Archive -LiteralPath $archivePath -DestinationPath $extractPath
    $extractedRoot = Join-Path $extractPath $archiveRootName
    if (-not (Test-Path -LiteralPath $extractedRoot -PathType Container)) {
        throw "The libvips archive did not contain the expected $archiveRootName directory."
    }

    New-Item -ItemType Directory -Path $stagingPath -Force | Out-Null
    Copy-Item -Path (Join-Path $extractedRoot "*") -Destination $stagingPath -Recurse -Force

    $importLibrary = Join-Path $stagingPath "lib\libvips.lib"
    if (-not (Test-Path -LiteralPath $importLibrary -PathType Leaf)) {
        throw "The required libvips.lib import library is missing from the libvips bundle."
    }
    Set-Content -LiteralPath (Join-Path $stagingPath ".focusless-libvips-version") `
        -Value $libvipsVersion -Encoding ASCII

    $destinationParent = Split-Path -Parent $Destination
    New-Item -ItemType Directory -Path $destinationParent -Force | Out-Null
    if (Test-Path -LiteralPath $Destination) {
        Move-Item -LiteralPath $Destination -Destination $backupPath
    }
    try {
        Move-Item -LiteralPath $stagingPath -Destination $Destination
    }
    catch {
        if ((Test-Path -LiteralPath $backupPath) -and -not (Test-Path -LiteralPath $Destination)) {
            Move-Item -LiteralPath $backupPath -Destination $Destination
        }
        throw
    }
    if (Test-Path -LiteralPath $backupPath) {
        Remove-Item -LiteralPath $backupPath -Recurse -Force
    }

    Write-Host "libvips $libvipsVersion is ready at $Destination"
    Write-Output $Destination
}
finally {
    if (Test-Path -LiteralPath $stagingPath) {
        Remove-Item -LiteralPath $stagingPath -Recurse -Force
    }
    if (Test-Path -LiteralPath $temporaryRoot) {
        Remove-Item -LiteralPath $temporaryRoot -Recurse -Force
    }
}
