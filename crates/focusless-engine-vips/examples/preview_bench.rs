use std::{
    env,
    error::Error,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use focusless_core::{Operation, PreviewRequest, Viewport};
use focusless_engine_vips::{EngineEvent, EngineWorker};

fn main() -> Result<(), Box<dyn Error>> {
    let source = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: preview_bench <image>")?;
    let engine = EngineWorker::start();
    let started = Instant::now();
    engine.request_preview(PreviewRequest {
        generation: 1,
        source_path: source,
        operations: vec![Operation::Exposure { ev: 0.75 }],
        viewport: Viewport::fit(1920, 1080),
    });

    loop {
        if started.elapsed() > Duration::from_secs(60) {
            return Err("preview timed out after 60 seconds".into());
        }
        match engine.try_event() {
            Some(EngineEvent::PreviewReady(Ok(result))) => {
                println!(
                    "preview={}x{} bytes={} elapsed_ms={}",
                    result.width,
                    result.height,
                    result.rgba8.len(),
                    started.elapsed().as_millis()
                );
                return Ok(());
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
