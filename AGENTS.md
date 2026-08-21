# Focusless Edit agent guide

This file applies to the entire repository. Read it together with
`README.md`, `CONTRIBUTING.md`, `docs/development-setup.md`, and
`docs/architecture.md` before changing production code.

## Product and communication

- Focusless Edit is a personal, Linux-first desktop photo editor. Keep it
  simple to use while maintaining professional internal architecture.
- Speak Turkish with the current owner unless asked otherwise.
- Keep every application label, dialog, status message, log entry, code
  identifier, comment, test, commit message, and document in English.
- The owner chooses the UI and feature scope. Do not add speculative controls
  or redesign the interface without discussing the behavior first.
- When practical, launch the release application after a completed UI feature
  so the owner can inspect it.

## Repository safety

- User photos and `.focusless` projects are local data, not source assets.
  Never commit files below `/focusless/`, arbitrary personal photos, recovery
  files, or `*.focusless` files.
- Preserve unrelated working-tree changes. Never use destructive Git commands
  to clean the repository.
- Do not edit generated files below `target/`.

## Architecture boundaries

- `focusless-core`: serializable document model, operation validation,
  undo/redo commands, and renderer contracts. It must not depend on Slint,
  file dialogs, or libvips.
- `focusless-storage`: source fingerprints and durable `.focusless` JSON
  persistence with relative paths and atomic replacement.
- `focusless-engine-vips`: decode, color management, image operations,
  viewport preview, and full-resolution export. libvips objects stay on the
  dedicated render worker.
- `focusless-app`: Slint presentation, native dialogs, controller
  transactions, autosave scheduling, and worker coordination. Do not expose
  libvips types to this crate.

Heavy rendering and disk synchronization must never run on the UI thread.
Preview requests use monotonically increasing generations; stale results must
not replace newer ones.

## Non-negotiable image pipeline

The canonical render order is:

1. Decode and honor EXIF orientation.
2. Convert the embedded source ICC profile to sRGB with LittleCMS; treat
   untagged RGB as sRGB.
3. Decode the sRGB transfer curve into 32-bit float linear scRGB.
4. Apply quarter-turn rotation, auto-cropped straighten rotation, and
   normalized crop.
5. Apply denoise, white balance, exposure, shadows/highlights, contrast, tone
   curve, and saturation in the documented order.
6. Apply sharpness to OKLab lightness, then vignette, then add the frame
   in linear light, before preview resize or output conversion.
7. Encode to display-referred sRGB and embed the sRGB ICC profile in exported
   JPEG, PNG, and WebP files.

Never apply light-based math directly to gamma-encoded JPEG samples. Preserve
alpha separately and avoid clipping extended-range float values in
intermediate stages unless the operation explicitly requires it. Color-science
changes require numerical reference tests.

## Document and interaction rules

- The current project schema is version 19. Old versions 1–18 must continue to
  load. Any semantic or serialized-model change requires a schema increment
  and an explicit migration test.
- Every document-changing action must support undo/redo. Group continuous
  slider movement into one command at release.
- The tone curve has fixed endpoints and three two-dimensional interior
  points. Interior input positions remain ordered; output values may cross.
  Drag release commits automatically. `Done` only closes the overlay.
- `Save` and `Save As` write editable `.focusless` projects. `Export` creates
  the rendered JPEG, PNG, or WebP.
- Autosave is debounced and atomic. A manual save must remain newer than any
  queued autosave snapshot.

## Slint and Linux details

- Use `preferred-width` and `preferred-height` for initial window size. Do not
  bind the root `Window.width` or `Window.height` to fixed values; maximized
  content must expand.
- In a Slint repetition such as `for item[index] in model`, `item` is the model
  value and `index` starts at zero. Use the correct one for grid positions.
- Native dialogs use `rfd` with XDG Portal and Zenity fallback. Ubuntu/WSL
  development machines must install `zenity`; otherwise Import, Save As, and
  Export silently fail inside the upstream backend.
- UI callbacks should express user intent. Image processing belongs in the
  engine, not in `.slint` files or the controller.

## Required checks

Run all of these before handing off a change:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --release --locked
git diff --check
```

For performance-sensitive renderer changes, also run the benchmark documented
in `docs/performance.md` and record source dimensions, viewport dimensions,
wall time, render time, and peak RSS.

## Feature workflow

For a new editing operation, follow `docs/adding-an-operation.md`. A feature is
not complete until its core parameters, validation, undo/redo transaction,
schema compatibility, renderer implementation, UI/controller connection,
autosave behavior, known-pixel test, alpha test, and export path all agree.
