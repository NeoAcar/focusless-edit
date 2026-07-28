# Contributing to Focusless Edit

## Getting started

1. Install the Ubuntu dependencies listed in the README.
2. Run `cargo test --workspace`.
3. Make the change in a small, single-purpose branch.

## Architecture rules

- The UI crate must not use libvips types directly.
- `focusless-core` must not depend on file dialogs, Slint, or libvips.
- User actions that modify the document must produce undo/redo commands.
- Every project schema change requires a version and compatibility test.
- Heavy image and disk operations must not run on the UI thread.
- A new editing operation is incomplete without persistence, rendering, and
  test coverage.
- Image math must name its color space and transfer function. Operations that
  model light must run in the 32-bit float linear working space, not directly
  on gamma-encoded display values.
- Color-science changes require numerical reference tests. Approximate methods
  must be explicitly documented and must not silently replace a physically or
  mathematically correct implementation.

## Before submitting

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

For performance-related changes, include the image dimensions, peak memory,
and before/after timings in the change description.
