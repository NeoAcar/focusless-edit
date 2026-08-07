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

Measured on July 31, August 3, and August 7, 2026, on Windows 11 Pro with 31.2 GiB RAM,
an AMD Ryzen 7 6800H, and libvips 8.18.2. Values are medians from three
release-mode runs:

- Source: synthetic `10000 × 10000` JPEG, 100 MP
- Output viewport: `1920 × 1080` RGBA8
- Initial color-managed proxy preview: `989 ms`
- White balance: `56 ms`
- Exposure: `50 ms`
- Contrast: `197 ms`
- Shadows and highlights: `492 ms`
- Tone curve: `84 ms`
- Saturation: `154 ms`
- Matrix look: `259 ms`
- Sharpness: `181 ms`
- Crop: `44 ms`
- Straighten rotation: `125 ms`
- Frame: `63 ms`
- Whole twelve-preview process wall time: `2.555 s`
- Peak resident memory: `862.7 MiB` in a separate instrumented run

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
