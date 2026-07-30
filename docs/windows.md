# Windows development and packaging

Focusless Edit supports native 64-bit Windows builds with the MSVC Rust
toolchain. The application uses native Win32 file dialogs and ships as a
portable directory containing the executable and its libvips runtime DLLs.
No project dependency needs a system-wide installation.

## Prerequisites

Install these once:

1. [Git for Windows](https://git-scm.com/download/win)
2. [Visual Studio Build Tools](https://visualstudio.microsoft.com/downloads/)
   with the **Desktop development with C++** workload and a Windows SDK
3. [Rustup](https://rustup.rs/)

Use a native **x64 Native Tools Command Prompt for VS 2022** or a PowerShell
window where the MSVC build tools are available. Confirm the environment:

```powershell
rustup default stable-x86_64-pc-windows-msvc
rustc --version
cargo --version
where.exe link
```

The checked-in `rust-toolchain.toml` selects Rust 1.97.1, `rustfmt`, and
`clippy` inside the repository.

## Clone and build

```powershell
git clone https://github.com/NeoAcar/focusless-edit.git
Set-Location focusless-edit
.\scripts\windows\build.ps1
```

The build script:

- downloads the pinned official libvips 8.18.2 x64 bundle;
- verifies its SHA-256 checksum before use;
- configures DLL and linker search paths for the current process;
- runs formatting, lint, workspace tests, and the release build.

The dependency bundle is stored under `.local\windows\libvips` and is ignored
by Git. Re-running the script reuses a complete matching installation.

If PowerShell blocks repository scripts, allow them only for the current
process and retry:

```powershell
Set-ExecutionPolicy -Scope Process -ExecutionPolicy Bypass
.\scripts\windows\build.ps1
```

## Run during development

After `build.ps1` completes in the same PowerShell process:

```powershell
.\target\release\focusless-edit.exe
```

In a new PowerShell window, load the native dependency environment first:

```powershell
. .\scripts\windows\environment.ps1
cargo run -p focusless-app
```

The leading dot and space dot-source `environment.ps1`, so its `PATH` and
`LIB` changes remain available to subsequent Cargo commands.

Useful development variants:

```powershell
.\scripts\windows\build.ps1 -Debug -SkipChecks
.\scripts\windows\build.ps1 -Package
```

## Create a portable package

```powershell
.\scripts\windows\package.ps1
```

This creates:

```text
dist\Focusless-Edit-windows-x64\
dist\Focusless-Edit-windows-x64.zip
```

The directory includes `focusless-edit.exe`, the libvips runtime DLLs and
modules, the libvips license, and this Windows guide. It can be copied to
another 64-bit Windows machine and run without Rust or a separate libvips
installation. Keep the DLLs beside the executable.

The portable build is currently unsigned and has no installer. Windows may
therefore show a SmartScreen warning when the archive came from another
computer.

## Color profile behavior

Focusless Edit first checks `FOCUSLESS_SRGB_PROFILE`, then looks beside the
executable, and finally uses the standard Windows profile:

```text
%SystemRoot%\System32\spool\drivers\color\sRGB Color Space Profile.icm
```

To use another valid sRGB profile explicitly:

```powershell
$env:FOCUSLESS_SRGB_PROFILE = "C:\absolute\path\to\sRGB.icc"
```

## Logs and recovery

On Windows, application state is below the current user's local application
data directory. The exact root is reported by Windows, normally under:

```text
%LOCALAPPDATA%\Focusless\Focusless Edit\
```

Daily `focusless.log` files contain runtime errors. Unsaved work is stored in
the `recovery\untitled.focusless` project below the same application data
area.

## Troubleshooting

### A DLL is missing when the application starts

For development, dot-source `environment.ps1` before running the executable.
For a portable build, launch the executable inside the package directory and
do not separate it from its DLLs.

### The linker cannot find `vips.lib`

Confirm that the environment script was dot-sourced and the import library
exists:

```powershell
. .\scripts\windows\environment.ps1
$env:LIB
Test-Path .\.local\windows\libvips\lib\libvips.lib
where.exe link
```

If `link.exe` is unavailable, open an x64 Native Tools shell or add the Visual
Studio C++ workload.

### The sRGB profile is reported missing

Confirm the standard profile exists:

```powershell
Test-Path "$env:SystemRoot\System32\spool\drivers\color\sRGB Color Space Profile.icm"
```

If Windows stores it elsewhere, set `FOCUSLESS_SRGB_PROFILE` to the absolute
profile path.

### Re-download the native dependency bundle

Remove only the repository-local bundle and run bootstrap again:

```powershell
Remove-Item -LiteralPath .\.local\windows\libvips -Recurse -Force
.\scripts\windows\bootstrap-libvips.ps1
```

The bootstrap script refuses broad root and repository-root destinations and
verifies the downloaded archive before replacing an existing local bundle.
