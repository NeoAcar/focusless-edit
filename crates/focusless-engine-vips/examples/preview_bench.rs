use std::{
    env,
    error::Error,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use focusless_core::{
    CropRect, FrameColor, Operation, PreviewRequest, ShadowsHighlights, ToneCurve, Viewport,
    WhiteBalance,
};
use focusless_engine_vips::{EngineEvent, EngineWorker};

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args_os().skip(1);
    let source = arguments.next().map(PathBuf::from).ok_or(
        "usage: preview_bench <image> \
             [--frame-sequence|--adjustment-sequence|--cached-downstream-sequence|--zoom-sequence]",
    )?;
    let mode = arguments.next();
    let frame_sequence = mode
        .as_deref()
        .is_some_and(|value| value == "--frame-sequence");
    let adjustment_sequence = mode
        .as_deref()
        .is_some_and(|value| value == "--adjustment-sequence");
    let cached_downstream_sequence = mode
        .as_deref()
        .is_some_and(|value| value == "--cached-downstream-sequence");
    let zoom_sequence = mode
        .as_deref()
        .is_some_and(|value| value == "--zoom-sequence");
    let engine = EngineWorker::start();
    let started = Instant::now();
    engine.request_preview(PreviewRequest {
        generation: 1,
        source_path: source.clone(),
        operations: vec![Operation::Exposure { ev: 0.75 }],
        viewport: Viewport::fit(1920, 1080),
    });
    let first = wait_for_preview(&engine, started)?;
    println!(
        "preview={}x{} bytes={} elapsed_ms={}",
        first.width,
        first.height,
        first.rgba8.len(),
        started.elapsed().as_millis()
    );

    if frame_sequence {
        let frame_started = Instant::now();
        engine.request_preview(PreviewRequest {
            generation: 2,
            source_path: source.clone(),
            operations: vec![
                Operation::Exposure { ev: 0.75 },
                Operation::Frame {
                    width_pct: 20.0,
                    color: FrameColor::BLACK,
                },
            ],
            viewport: Viewport::fit(1920, 1080),
        });
        let framed = wait_for_preview(&engine, frame_started)?;
        println!(
            "frame_preview={}x{} bytes={} elapsed_ms={}",
            framed.width,
            framed.height,
            framed.rgba8.len(),
            frame_started.elapsed().as_millis()
        );
    }
    if adjustment_sequence {
        let cases = [
            (
                "white_balance",
                vec![Operation::WhiteBalance {
                    adjustment: WhiteBalance {
                        temperature: 35.0,
                        tint: -12.0,
                    },
                }],
            ),
            ("exposure", vec![Operation::Exposure { ev: 1.25 }]),
            ("contrast", vec![Operation::Contrast { amount: 40.0 }]),
            (
                "shadows_highlights",
                vec![Operation::ShadowsHighlights {
                    adjustment: ShadowsHighlights {
                        shadows: 45.0,
                        highlights: 35.0,
                    },
                }],
            ),
            (
                "tone_curve",
                vec![Operation::ToneCurve {
                    curve: ToneCurve {
                        shadows: 0.18,
                        midtones: 0.56,
                        highlights: 0.84,
                        ..ToneCurve::IDENTITY
                    },
                }],
            ),
            ("saturation", vec![Operation::Saturation { amount: 45.0 }]),
            ("matrix", vec![Operation::Matrix { enabled: true }]),
            ("sharpness", vec![Operation::Sharpness { amount: 120.0 }]),
            (
                "crop",
                vec![Operation::Crop {
                    rect: CropRect {
                        x: 0.1,
                        y: 0.1,
                        width: 0.8,
                        height: 0.8,
                    },
                }],
            ),
            ("rotation", vec![Operation::Straighten { degrees: 12.0 }]),
            (
                "frame",
                vec![Operation::Frame {
                    width_pct: 20.0,
                    color: FrameColor::BLACK,
                }],
            ),
        ];
        for (index, (name, operations)) in cases.into_iter().enumerate() {
            let feature_started = Instant::now();
            engine.request_preview(PreviewRequest {
                generation: index as u64 + 2,
                source_path: source.clone(),
                operations,
                viewport: Viewport::fit(1920, 1080),
            });
            let result = wait_for_preview(&engine, feature_started)?;
            println!(
                "feature_preview={} size={}x{} elapsed_ms={}",
                name,
                result.width,
                result.height,
                feature_started.elapsed().as_millis()
            );
        }
    }
    if cached_downstream_sequence {
        let shadows_highlights = Operation::ShadowsHighlights {
            adjustment: ShadowsHighlights {
                shadows: 45.0,
                highlights: 35.0,
            },
        };
        let cache_started = Instant::now();
        engine.request_preview(PreviewRequest {
            generation: 2,
            source_path: source.clone(),
            operations: vec![shadows_highlights],
            viewport: Viewport::fit(1920, 1080),
        });
        let result = wait_for_preview(&engine, cache_started)?;
        println!(
            "cached_stage_prime size={}x{} elapsed_ms={}",
            result.width,
            result.height,
            cache_started.elapsed().as_millis()
        );

        let cases = [
            (
                "tone_curve",
                Operation::ToneCurve {
                    curve: ToneCurve {
                        shadows: 0.18,
                        midtones: 0.56,
                        highlights: 0.84,
                        ..ToneCurve::IDENTITY
                    },
                },
            ),
            ("saturation", Operation::Saturation { amount: 45.0 }),
            ("matrix", Operation::Matrix { enabled: true }),
            ("sharpness", Operation::Sharpness { amount: 120.0 }),
            (
                "frame",
                Operation::Frame {
                    width_pct: 20.0,
                    color: FrameColor::BLACK,
                },
            ),
        ];
        for (index, (name, operation)) in cases.into_iter().enumerate() {
            let feature_started = Instant::now();
            engine.request_preview(PreviewRequest {
                generation: index as u64 + 3,
                source_path: source.clone(),
                operations: vec![shadows_highlights, operation],
                viewport: Viewport::fit(1920, 1080),
            });
            let result = wait_for_preview(&engine, feature_started)?;
            println!(
                "cached_downstream_preview={} size={}x{} elapsed_ms={}",
                name,
                result.width,
                result.height,
                feature_started.elapsed().as_millis()
            );
        }
    }
    if zoom_sequence {
        for (index, factor) in [1.15_f32, 1.15_f32.powi(2), 1.15_f32.powi(3)]
            .into_iter()
            .enumerate()
        {
            let zoom_started = Instant::now();
            engine.request_preview(PreviewRequest {
                generation: index as u64 + 2,
                source_path: source.clone(),
                operations: vec![Operation::Exposure { ev: 0.75 }],
                viewport: Viewport {
                    output_width: 1920,
                    output_height: 1080,
                    zoom: first.effective_zoom * factor,
                    center_x: 0.5,
                    center_y: 0.5,
                },
            });
            let result = wait_for_preview(&engine, zoom_started)?;
            println!(
                "zoom_preview={}x fit size={}x{} elapsed_ms={}",
                factor,
                result.width,
                result.height,
                zoom_started.elapsed().as_millis()
            );
        }
    }
    Ok(())
}

fn wait_for_preview(
    engine: &EngineWorker,
    started: Instant,
) -> Result<focusless_core::RenderResult, Box<dyn Error>> {
    loop {
        if started.elapsed() > Duration::from_secs(60) {
            return Err("preview timed out after 60 seconds".into());
        }
        match engine.try_event() {
            Some(EngineEvent::PreviewReady(Ok(result))) => {
                return Ok(result);
            }
            Some(EngineEvent::PreviewReady(Err(error)) | EngineEvent::Fatal(error)) => {
                return Err(error.into());
            }
            Some(
                EngineEvent::Inspected { .. }
                | EngineEvent::ExportStarted { .. }
                | EngineEvent::ClipboardReady(..)
                | EngineEvent::ExportFinished { .. },
            )
            | None => thread::sleep(Duration::from_millis(2)),
        }
    }
}
