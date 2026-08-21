# Focusless Edit

Focusless Edit is a personal desktop photo editor focused on a simple
interface, non-destructive editing, and controlled memory usage with large
images.

The project currently contains its first working vertical slice:

- Import JPEG, PNG, and WebP images
- Fit, 100% view, zoom, and pan
- Live Temperature and Tint white-balance adjustment
- Perceptual Contrast adjustment from `-100` to `+100`
- Edge-aware Shadows and Highlights adjustments from `-100` to `+100`
- Separate luminance and color denoise adjustments from `0` to `100`
- Perceptual Saturation adjustment from `-100` to `+100`
- Luminance-only Sharpness adjustment from `0` to `1000`
- Live exposure adjustment from `-3 EV` to `+3 EV`
- Interactive five-point tone curve drawn directly over the photo
- Non-destructive ±45-degree straighten dial
- Interactive crop with move, edge/corner resize, Free mode, and aspect presets
- Adjustable export frame with five neutral color presets
- Adjustable luminance vignette from `0` to `100`
- Persistent undo/redo history
- Versioned `.focusless` project files
- Debounced, atomic autosave
- JPEG, PNG, and WebP export
- Viewport-based previews for 50–100 MP images

RAW files, layers, masks, and photo catalogs are not implemented yet.

## Technology

- Rust 1.97
- [Slint](https://slint.dev/) desktop UI
- [libvips](https://www.libvips.org/) image engine
- LittleCMS-backed ICC color management with a 32-bit float linear scRGB
  working space
- Native Linux runtime with X11 and Wayland support
- Native Windows x64 runtime and portable packaging

The UI, document model, persistence, and image engine live in separate crates.
See the [architecture document](docs/architecture.md) for details and the
[performance notes](docs/performance.md) for the large-image baseline.

New developers and AI collaborators should follow the complete
[development setup and handoff guide](docs/development-setup.md). Repository
rules for coding agents are in [AGENTS.md](AGENTS.md).

Native Windows development, release builds, and portable packages are covered
in the [Windows guide](docs/windows.md).

## Ubuntu 24.04 setup

```bash
sudo apt update
sudo apt install -y \
  build-essential pkg-config libvips-dev icc-profiles-free \
  libfontconfig1-dev libxkbcommon-dev libxkbcommon-x11-dev \
  libwayland-dev libgl1-mesa-dev zenity
```

If Rust is not installed:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

The repository's `rust-toolchain.toml` selects the required toolchain and
components automatically.

Clone, verify, and build:

```bash
git clone https://github.com/NeoAcar/focusless-edit.git
cd focusless-edit
cargo test --workspace --locked
cargo build --workspace --release --locked
```

## Windows x64 setup

Install Git, Rustup, and Visual Studio Build Tools with the **Desktop
development with C++** workload. Then run from a native PowerShell window:

```powershell
git clone https://github.com/NeoAcar/focusless-edit.git
Set-Location focusless-edit
.\scripts\windows\build.ps1 -Package
```

The script downloads and verifies the pinned official libvips runtime, runs
the quality gate, builds the native application, and creates
`dist\Focusless-Edit-windows-x64.zip`. See the
[complete Windows guide](docs/windows.md) for development and
troubleshooting.

## Run

```bash
cargo run -p focusless-app
```

Run the optimized build:

```bash
./target/release/focusless-edit
```

Import a photo or project directly:

```bash
cargo run -p focusless-app -- /path/to/photo.jpg
cargo run -p focusless-app -- /path/to/edit.focusless
```

Enable more detailed logs:

```bash
RUST_LOG=debug cargo run -p focusless-app
```

On Linux, logs are stored in the `focuslessedit` directory below the XDG state
directory. Unsaved work uses a recovery project below the XDG data directory.

## Keyboard and mouse

| Action | Shortcut |
| --- | --- |
| Import | `Ctrl+O` |
| Save / Save As | `Ctrl+S` / `Ctrl+Shift+S` |
| Export | `Ctrl+E` |
| Copy edited image | `Ctrl+C` |
| Undo / Redo | `Ctrl+Z` / `Ctrl+Y` |
| Fit / 100% | `0` / `1` |
| Zoom | Mouse wheel |
| Pan | Drag the photo |
| Move crop | Drag inside the crop frame |
| Resize crop | Drag a crop-frame edge or corner |
| Shape tone curve | Drag any white control point |

The crop tool includes Free, 1:1, 4:3, and 16:9 modes plus a full-image reset.
Crop and straighten rotation are stored non-destructively in the project and
applied to full-resolution exports. Temperature and Tint use CAT16 chromatic
adaptation in linear RGB and preserve alpha. Contrast reshapes OKLab
lightness while leaving chroma and extended-range values intact. Shadows and
Highlights use an edge-aware guided filter over log OKLab lightness, preserving
fine detail, chroma, alpha, and extended-range values. Saturation scales OKLab
chroma while preserving perceptual lightness and hue. Denoise uses scale-aware
guided filters on OKLab lightness and chroma so fitted previews remain
consistent with full-resolution output. Sharpness uses a
thresholded unsharp mask on OKLab lightness to avoid color halos. The tone
curve works in linear RGB with fixed endpoints, three two-dimensional control
points, shape-preserving interpolation, and a live full-image preview. The
vignette darkens OKLab lightness radially after sharpening. The frame is
composited in linear light after vignette and is included in
full-resolution exports.

## Quality checks

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --release --locked
git diff --check
```

Before contributing, read [CONTRIBUTING.md](CONTRIBUTING.md) and the
[guide for adding an operation](docs/adding-an-operation.md).
