# Performance measurements

Repeat these measurements as features are added. The values are a regression
baseline, not a product guarantee.

## 100 MP viewport preview

Measured on July 27, 2026, under WSL2 Ubuntu 24.04 with 24 GiB RAM and
libvips 8.15.1 using the ICC-managed 32-bit float linear scRGB pipeline:

- Source: synthetic `10000 × 10000` JPEG, 100 MP
- Operation: `+0.75 EV`
- Output viewport: `1920 × 1080` RGBA8
- Focusless render time: `1039 ms`
- Process wall time: `1.21 s`
- Peak resident memory: `339404 KiB` (approximately 331 MiB)
- UI thread blocking: none; the benchmark uses the same render-worker path

Reproduce the measurement:

```bash
vips black /tmp/focusless-100mp.jpg 10000 10000 --bands 3
cargo build -p focusless-engine-vips --example preview_bench --release
/usr/bin/time -v \
  target/release/examples/preview_bench /tmp/focusless-100mp.jpg
```

libvips may place large intermediate data on temporary storage instead of
memory. This run produced approximately 572 MiB of filesystem writes, a
deliberate tradeoff for controlled RAM usage. Future measurements with real
photos should track preview latency, disk writes, and close-zoom tile behavior
together.

## 100 MP interactive adjustments

Measured most recently on August 21, 2026, on Windows 11 Pro with 31.2 GiB
RAM, an AMD Ryzen 7 6800H, and libvips 8.18.2. Values are medians from three
release-mode runs:

- Source: synthetic `10000 × 10000` JPEG, 100 MP
- Output viewport: `1920 × 1080` RGBA8
- Initial color-managed proxy preview: `1071 ms`
- White balance: `54 ms`
- Exposure: `47 ms`
- Contrast: `125 ms`
- Shadows and highlights: `303 ms`
- Denoise: `516 ms`
- Tone curve: `67 ms`
- Saturation: `101 ms`
- Sharpness: `170 ms`
- Vignette: `210 ms`
- Crop: `19 ms`
- Straighten rotation: `75 ms`
- Frame: `30 ms`
- Whole thirteen-preview process wall time: `2.933 s`
- Peak resident memory: `631.2 MiB`

The first Shadows/Highlights implementation measured `1778 ms` for the same
case. Replacing its multi-level pseudo-Laplacian reconstruction with the
edge-aware guided filter reduced the median to `492 ms`, a `3.6×` speedup,
while retaining local masking, fine detail, alpha, and extended-range values.

The fit-preview path materializes one color-managed, 32-bit float linear proxy
at 1.5 times the fitted display resolution. Every interactive operation reuses
that source proxy. Sharpness radius scales with the proxy, while zoomed
previews and full-resolution exports continue to evaluate the canonical
pipeline from the source. The proxy changes latency only; exported pixels and
embedded color profiles are unchanged.

Reproduce the measurement in PowerShell:

```powershell
.\.local\windows\libvips\bin\vips.exe black `
  .local\focusless-bench-100mp.jpg 10000 10000 --bands 3
cargo build -p focusless-engine-vips --example preview_bench --release --locked
.\target\release\examples\preview_bench.exe `
  .local\focusless-bench-100mp.jpg --adjustment-sequence
```

## Shadows/Highlights downstream cache

Measured on August 11, 2026, on Windows 11 Pro with 31.2 GiB RAM, an AMD
Ryzen 7 6800H, and libvips 8.18.2:

- Source: `2048 x 2048` JPEG
- Output viewport: `1920 x 1080` RGBA8
- Initial proxy preview: `190 ms`
- Shadows/Highlights cache prime: `420 ms`
- Cached Tone Curve: `86 ms`
- Cached Saturation: `180 ms`
- Cached Matrix look: `185 ms`
- Cached Sharpness: `201 ms`
- Cached Frame: `48 ms`
- Whole process wall time: `2.151 s`
- Peak resident memory: `607.8 MiB`

The cache contains the materialized fit-preview result through the
Shadows/Highlights stage. Later operations reuse those pixels; geometry,
Denoise, White Balance, Exposure, and Shadows/Highlights changes rebuild the
stage. Contrast is downstream and now reuses the cache. Full-resolution zoom
previews and exports do not use this cache.

Reproduce the measurement in PowerShell:

```powershell
cargo build -p focusless-engine-vips --example preview_bench --release --locked
.\target\release\examples\preview_bench.exe `
  path\to\photo.jpg --cached-downstream-sequence
```

## Denoise downstream cache

Measured on August 21, 2026, on Windows 11 Pro with 31.2 GiB RAM, an AMD
Ryzen 7 6800H, and libvips 8.18.2. Values are medians from three release-mode
runs:

- Source: synthetic `10000 × 10000` JPEG, 100 MP
- Output viewport: `1920 × 1080` RGBA8
- Initial color-managed proxy preview: `1058 ms`
- Denoise cache prime: `520 ms`
- Cached Exposure: `45 ms`
- Cached Contrast: `166 ms`
- Cached Tone Curve: `72 ms`
- Cached Saturation: `118 ms`
- Cached Sharpness: `124 ms`
- Cached Vignette: `199 ms`
- Cached Frame: `40 ms`
- Whole process wall time: `2.373 s`
- Peak resident memory: `601.5 MiB`

When Denoise is active without Shadows/Highlights, the fit-preview cache ends
immediately after Denoise. White Balance, Exposure, and every later adjustment
reuse the denoised pixels. Changing geometry or either Denoise amount rebuilds
the stage. Denoise spatial radii and chroma subsampling scale with the proxy;
full-resolution zoom previews and exports still use full-resolution radii.

Reproduce the measurement in PowerShell:

```powershell
cargo build -p focusless-engine-vips --example preview_bench --release --locked
.\target\release\examples\preview_bench.exe `
  .local\focusless-bench-100mp.jpg --denoise-downstream-sequence
```
