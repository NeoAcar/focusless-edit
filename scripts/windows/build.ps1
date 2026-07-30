[CmdletBinding()]
param(
    [switch]$Debug,
    [switch]$SkipChecks,
    [switch]$Package
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
. (Join-Path $PSScriptRoot "environment.ps1")
Push-Location $repositoryRoot
try {
    if (-not $SkipChecks) {
        & cargo fmt --all -- --check
        if ($LASTEXITCODE -ne 0) { throw "cargo fmt failed." }

        & cargo clippy --workspace --all-targets --locked -- -D warnings
        if ($LASTEXITCODE -ne 0) { throw "cargo clippy failed." }

        & cargo test --workspace --locked
        if ($LASTEXITCODE -ne 0) { throw "cargo test failed." }
    }

    $buildArguments = @("build", "--workspace", "--locked")
    if (-not $Debug) {
        $buildArguments += "--release"
    }
    & cargo @buildArguments
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed." }

    if ($Package) {
        if ($Debug) {
            throw "Portable packages are created from release builds only."
        }
        & (Join-Path $PSScriptRoot "package.ps1") -SkipBuild
        if ($LASTEXITCODE -ne 0) { throw "Windows packaging failed." }
    }
}
finally {
    Pop-Location
}
