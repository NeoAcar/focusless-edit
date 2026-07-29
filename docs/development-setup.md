# Development setup and handoff

This guide takes a new Ubuntu or WSL2 developer from an empty machine to a
tested release build. Ubuntu 24.04 is the reference environment. WSL2 requires
WSLg or another working X11/Wayland display server to show the desktop UI.

## 1. Install native dependencies

```bash
sudo apt update
sudo apt install -y \
  build-essential pkg-config libvips-dev icc-profiles-free \
  libfontconfig1-dev libxkbcommon-dev libxkbcommon-x11-dev \
  libwayland-dev libgl1-mesa-dev zenity
```

Why the less obvious packages are required:

- `libvips-dev`: decoding, image processing, preview rendering, and export.
- `icc-profiles-free`: the sRGB ICC profile required by the managed color
  pipeline.
- `zenity`: native Import, Save As, and Export dialogs when an XDG Desktop
  Portal service is not available, including the usual WSLg setup.
- Font, XKB, Wayland, and OpenGL packages: Slint's Linux window and renderer
  backends.

Verify the important runtime pieces:

```bash
pkg-config --modversion vips
test -f /usr/share/color/icc/sRGB.icc
zenity --version
```

If the distribution stores its sRGB profile elsewhere, set an explicit path:

```bash
export FOCUSLESS_SRGB_PROFILE=/absolute/path/to/sRGB.icc
```

## 2. Install Rust

Skip the installer if `rustup` is already available.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

The checked-in `rust-toolchain.toml` selects Rust 1.97.1 with `rustfmt` and
`clippy`. Confirm it from the repository after cloning:

```bash
rustc --version
cargo --version
```

## 3. Clone and bootstrap

```bash
git clone https://github.com/NeoAcar/focusless-edit.git
cd focusless-edit
cargo fetch --locked
cargo test --workspace --locked
cargo build --workspace --release --locked
```

`cargo fetch` needs internet access the first time. Subsequent locked builds
use the versions recorded in `Cargo.lock`.

## 4. Run the application

Run the optimized application:

```bash
./target/release/focusless-edit
```

Import a source directly:

```bash
./target/release/focusless-edit /absolute/path/to/photo.jpg
./target/release/focusless-edit /absolute/path/to/edit.focusless
```

For faster edit/compile cycles:

```bash
cargo run -p focusless-app
```

`Save` and `Save As` create editable `.focusless` project files. `Export`
creates JPEG, PNG, or WebP output.

## 5. Run the quality gate

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --release --locked
git diff --check
```

CI runs the same formatting, lint, test, and release-build checks on Ubuntu
24.04.

## 6. Logs and recovery

Default Linux locations are:

```text
~/.local/state/focuslessedit/focusless.log.YYYY-MM-DD
~/.local/share/focuslessedit/recovery/untitled.focusless
```

The application automatically opens the recovery project at startup when no
command-line path is supplied. Do not commit recovery files or personal
photos.

Enable detailed logs:

```bash
RUST_LOG=debug ./target/release/focusless-edit
```

## 7. Troubleshooting

### Import, Save As, or Export does nothing

Check Zenity first:

```bash
command -v zenity
zenity --version
zenity --file-selection
```

Install it if missing:

```bash
sudo apt install -y zenity
```

`rfd` tries XDG Desktop Portal before its Zenity fallback. A portal error can
therefore appear in the log even when the Zenity dialog subsequently works.
If `zenity --file-selection` also cannot display a window, verify `DISPLAY`,
`WAYLAND_DISPLAY`, and the WSLg/desktop session.

### The engine reports that an sRGB profile is missing

```bash
sudo apt install -y icc-profiles-free
ls -l /usr/share/color/icc/sRGB.icc
```

Or set `FOCUSLESS_SRGB_PROFILE` to a valid sRGB ICC file.

### libvips cannot be found during build

```bash
sudo apt install -y libvips-dev pkg-config
pkg-config --cflags --libs vips
```

### The window opens but rendering fails

Run with `RUST_LOG=debug`, inspect the current daily log, and confirm the source
format is JPEG, PNG, or WebP. RAW support is not implemented.

## 8. Git handoff

Before handing the repository to another developer or AI:

```bash
git status --short
git add -u
git add AGENTS.md docs/development-setup.md
git status --short
cargo test --workspace --locked
git commit -m "Document development setup and AI handoff"
git push origin main
```

Review the staged list before committing. The repository ignores the local
`focusless/` photo directory and all `.focusless` project files, but personal
media should still be checked manually.

The next AI should begin by reading `AGENTS.md`, then
`docs/architecture.md`, `CONTRIBUTING.md`, and
`docs/adding-an-operation.md`.
