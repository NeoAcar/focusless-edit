# Architecture

## Layers

```text
Slint UI
   │ user intent / presentation model
   ▼
focusless-app ───────────────► focusless-storage
   │                              │
   │ PreviewRequest               │ versioned JSON and atomic writes
   ▼                              ▼
focusless-engine-vips ◄──── focusless-core
   │                        document, operations, history, render contracts
   ▼
libvips worker + libvips CPU thread pool
```

The workspace contains four production crates:

- `focusless-core`: UI- and renderer-independent document model, operation
  definitions, undo/redo history, and render request types.
- `focusless-storage`: source fingerprints, relative source paths,
  `.focusless` JSON loading/writing, and atomic file replacement.
- `focusless-engine-vips`: decode, ICC color management, EXIF orientation,
  rotation, crop, linear-light adjustment, visible viewport rendering, and
  full-resolution export.
- `focusless-app`: Slint presentation model, file dialogs, and worker
  coordination.

## Preview data flow

1. The UI changes an adjustment or the viewport.
2. The controller creates a `PreviewRequest` with an increasing `generation`.
3. Only the newest request that has not started reaches the libvips worker.
4. For fit previews, the worker materializes one color-managed linear source
   proxy and every interactive operation reuses it. Zoomed previews compute
   the visible source region from the full-resolution pipeline.
5. An RGBA8 pixel buffer returns to the UI thread.
6. The controller displays the result only when its generation is still the
   newest.

This prevents stale slider renders from replacing newer results. libvips
objects remain on one Focusless worker thread, while libvips uses its internal
CPU pool for image computation.

## Document and project format

`ProjectDocument` persists:

- Schema version (currently version 13; version 1–12 projects migrate on load)
- Source photo path and sampled BLAKE3 fingerprint
- Ordered non-destructive operation list
- Zoom and normalized center coordinates
- Up to 200 undo/redo commands

The source path may be written relative to the project file. Persistence writes
to a temporary file in the same directory, calls `sync_all`, and atomically
renames it. Automatic and manual saves run on a storage worker so slow disk
synchronization cannot block the UI.

Geometry and tonal operations use a stable render order: EXIF orientation,
ICC conversion to the working space, quarter-turn rotation, auto-cropped
straighten rotation, normalized crop, white balance, exposure, contrast, the
tone curve, saturation, then the optional Matrix look. A crop
rectangle is stored as normalized coordinates so it remains independent of
source resolution. Sharpness follows the tonal operations, and the frame is
added last before preview resizing or output conversion. Rotating an existing
crop transforms the rectangle with the image and records both changes as one
undoable command.

The color pipeline converts an embedded source profile to sRGB with LittleCMS,
using relative colorimetric intent and black-point compensation. An untagged
RGB source is explicitly treated as sRGB. The engine then decodes the transfer
curve to 32-bit float linear scRGB. Exposure multiplies color channels by
`2^EV` in linear light while alpha remains separate and unchanged. Resampling
also occurs in this linear working space. Preview and export convert back to
display-referred sRGB, and exported JPEG, PNG, and WebP files embed the sRGB ICC
profile.

Temperature is mapped symmetrically in reciprocal color temperature (mired)
around D65, while Tint moves perpendicular to the Planckian locus in CIE 1960
UCS. The target white is applied with full CAT16 chromatic adaptation through
linear sRGB and XYZ matrices. The transform preserves the reference-white
luminance, leaves alpha separate, and retains extended-range floating-point
channel values until output conversion.

The tone curve is a shape-preserving piecewise cubic Hermite spline over five
points. Its endpoints stay at `(0, 0)` and `(1, 1)`; users may move the three
interior points in both dimensions. Interior input positions remain ordered so
the result is a function, while output values may cross. Per-segment tangent
limiting prevents interpolation overshoot. The renderer applies a 16-bit
lookup table to the linear RGB channels, preserves alpha, and leaves
extended-range values outside `0..1` unchanged.

Contrast applies a symmetric power curve around `0.5` to OKLab lightness. The
`-100..+100` control ranges from a constant mid-gray lightness through identity
to stronger separation of shadows and highlights. OKLab chroma, alpha, and
extended-range lightness values outside `0..1` remain unchanged.

Saturation converts linear sRGB to OKLab, scales only the `a` and `b` chroma
axes, then converts back to linear sRGB. The `-100..+100` control maps to a
`0..2` chroma multiplier, so `-100` is neutral gray, `0` is identity, and
`+100` doubles chroma. Lightness and hue remain unchanged in OKLab, alpha stays
separate, and no intermediate gamut clipping is performed.

The fixed Matrix look retains 30% of existing OKLab chroma. It then smoothly
interpolates exact `a,b` biases across lightness anchors: shadows
`(-0.018, -0.004)`, midtones `(-0.035, +0.006)`, and highlights
`(-0.006, +0.018)`. This creates green/cyan shadows, stronger green midtones,
and slightly yellow highlights without changing OKLab lightness or alpha.
Lightness is bounded only for selecting the tonal bias; extended-range color
values are not clipped.

Sharpness applies a full-resolution unsharp mask only to OKLab lightness:
`L' = L + gain × (L - GaussianBlur(L, 1 px))`. The `0..300` control maps to a
`0..3` gain. Detail below a fixed `0.003` OKLab-lightness threshold is left
unchanged to avoid amplifying low-level noise. Chroma and alpha remain
separate, which prevents colored sharpening halos.

The frame width is a percentage of the shorter cropped image dimension. Its
sRGB color preset is decoded to linear light before an opaque border is added
after sharpening. Preview rendering caches the color-managed linear source
proxy, so frame changes do not repeat source decoding or ICC conversion.
Export still evaluates the complete full-resolution pipeline.

Straighten rotation is limited to `-45..+45` degrees. Resampling uses
premultiplied alpha in linear light, then crops to the largest centered
axis-aligned rectangle without empty corners. One drag gesture creates one
undoable command.

Crop editing temporarily renders the complete rotated photo and draws the
interactive frame in Slint. Apply creates one history command; Cancel restores
the original rectangle without modifying history or autosave state.

## Thread model

- UI thread: Slint event loop and small state transitions.
- Render worker: owns libvips objects and handles inspection, preview, and
  export.
- libvips pool: distributes tile and scanline computation across CPU cores.
- Storage worker: serializes project writes and retains only the newest queued
  autosave snapshot.
- Linux-only Tokio runtime: supports the XDG portal file dialog's D-Bus
  operations.

Windows uses `rfd`'s native Win32 dialogs. Official libvips runtime DLLs are
kept beside the packaged executable, while development shells add the pinned
bundle's `bin` and `lib` directories to the process environment. Project saves
and exports use Windows replacement semantics so overwriting an existing
destination remains atomic and durable.

The renderer boundary uses core request/result contracts, so a GPU backend can
be added later if measurements justify it.
