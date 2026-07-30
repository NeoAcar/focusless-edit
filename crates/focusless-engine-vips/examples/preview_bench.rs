use std::{
    env,
    error::Error,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use focusless_core::{FrameColor, Operation, PreviewRequest, Viewport};
use focusless_engine_vips::{EngineEvent, EngineWorker};

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args_os().skip(1);
    let source = arguments
        .next()
        .map(PathBuf::from)
        .ok_or("usage: preview_bench <image> [--frame-sequence]")?;
    let frame_sequence = arguments
        .next()
        .is_some_and(|value| value == "--frame-sequence");
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
            source_path: source,
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
                | EngineEvent::ExportFinished { .. },
            )
            | None => thread::sleep(Duration::from_millis(2)),
        }
    }
}
