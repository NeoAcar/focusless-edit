# Focusless Edit

Focusless Edit is a personal desktop photo editor focused on a simple
interface, non-destructive editing, and controlled memory usage with large
images.

The project currently contains its first working vertical slice:

- Import JPEG, PNG, and WebP images
- Fit, 100% view, zoom, and pan
- Live exposure adjustment from `-5 EV` to `+5 EV`
- Interactive five-point tone curve drawn directly over the photo
- Non-destructive 90-degree rotation
- Interactive crop with move, edge/corner resize, Free mode, and aspect presets
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
- Linux-first runtime with X11 and Wayland support

The UI, document model, persistence, and image engine live in separate crates.
See the [architecture document](docs/architecture.md) for details and the
[performance notes](docs/performance.md) for the large-image baseline.

## Ubuntu 24.04 setup

```bash
sudo apt update
sudo apt install -y \
  build-essential pkg-config libvips-dev icc-profiles-free \
  libfontconfig1-dev libxkbcommon-dev libxkbcommon-x11-dev \
  libwayland-dev libgl1-mesa-dev
```

If Rust is not installed:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

The repository's `rust-toolchain.toml` selects the required toolchain and
components automatically.

## Run

```bash
cargo run -p focusless-app
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
| Undo / Redo | `Ctrl+Z` / `Ctrl+Y` |
| Fit / 100% | `0` / `1` |
| Zoom | Mouse wheel |
| Pan | Drag the photo |
| Move crop | Drag inside the crop frame |
| Resize crop | Drag a crop-frame edge or corner |
| Shape tone curve | Drag any blue control point vertically |

The crop tool includes Free, 1:1, 4:3, and 16:9 modes plus a full-image reset.
Crop and rotation are stored non-destructively in the project and applied to
full-resolution exports. The tone curve works in linear RGB with fixed
endpoints, three two-dimensional control points, shape-preserving
interpolation, and a live full-image preview.

## Quality checks

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
```

Before contributing, read [CONTRIBUTING.md](CONTRIBUTING.md) and the
[guide for adding an operation](docs/adding-an-operation.md).
