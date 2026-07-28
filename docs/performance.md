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
