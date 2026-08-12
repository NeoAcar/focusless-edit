//! libvips-backed rendering for Focusless Edit.
//!
//! All libvips objects are created and dropped on one dedicated worker thread.
//! This keeps the UI responsive and respects the Rust binding's thread-safety
//! constraints while libvips still uses its own internal worker pool.

mod vips_compat;

use std::{
    env, fs,
    mem::size_of,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select, unbounded};
use focusless_core::{
    CopyRequest, CopyResult, CropRect, ExportFormat, ExportRequest, FrameColor, Operation,
    PreviewRequest, RenderError, RenderResult, ShadowsHighlights, ToneCurve, Viewport,
    WhiteBalance,
};
use tracing::{debug, error};
use vips_compat::{VipsApp, VipsError, VipsImage, ops};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug)]
pub enum EngineCommand {
    Inspect { path: PathBuf },
    Export(ExportRequest),
    CopyClipboard(CopyRequest),
    Shutdown,
}

#[derive(Debug)]
pub enum EngineEvent {
    Fatal(RenderError),
    Inspected {
        path: PathBuf,
        result: Result<ImageInfo, RenderError>,
    },
    PreviewReady(Result<RenderResult, RenderError>),
    ExportStarted {
        destination: PathBuf,
    },
    ExportFinished {
        destination: PathBuf,
        result: Result<(), RenderError>,
    },
    ClipboardReady(Result<CopyResult, RenderError>),
}

/// UI-facing handle for the dedicated libvips worker.
pub struct EngineWorker {
    command_tx: Sender<EngineCommand>,
    preview_tx: Sender<PreviewRequest>,
    preview_rx_for_replacement: Receiver<PreviewRequest>,
    event_rx: Receiver<EngineEvent>,
    cancel_export: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl EngineWorker {
    pub fn start() -> Self {
        let (command_tx, command_rx) = unbounded();
        let (preview_tx, preview_rx) = bounded(1);
        let preview_rx_for_replacement = preview_rx.clone();
        let (event_tx, event_rx) = unbounded();
        let cancel_export = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel_export);
        let join = thread::Builder::new()
            .name("focusless-render".into())
            .spawn(move || worker_loop(command_rx, preview_rx, event_tx, worker_cancel))
            .expect("failed to create render worker");

        Self {
            command_tx,
            preview_tx,
            preview_rx_for_replacement,
            event_rx,
            cancel_export,
            join: Some(join),
        }
    }

    pub fn inspect(&self, path: PathBuf) {
        let _ = self.command_tx.send(EngineCommand::Inspect { path });
    }

    /// Enqueues the newest preview, replacing an older request that has not
    /// started yet. A preview already being rendered is discarded by its
    /// generation number in the application controller.
    pub fn request_preview(&self, request: PreviewRequest) {
        match self.preview_tx.try_send(request) {
            Ok(()) => {}
            Err(TrySendError::Full(request)) => {
                let _ = self.preview_rx_for_replacement.try_recv();
                let _ = self.preview_tx.try_send(request);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    pub fn export(&self, request: ExportRequest) {
        self.cancel_export.store(false, Ordering::Release);
        let _ = self.command_tx.send(EngineCommand::Export(request));
    }

    pub fn cancel_export(&self) {
        self.cancel_export.store(true, Ordering::Release);
    }

    pub fn copy_clipboard(&self, request: CopyRequest) {
        self.cancel_export.store(false, Ordering::Release);
        let _ = self.command_tx.send(EngineCommand::CopyClipboard(request));
    }

    #[must_use]
    pub fn try_event(&self) -> Option<EngineEvent> {
        self.event_rx.try_recv().ok()
    }
}

impl Drop for EngineWorker {
    fn drop(&mut self) {
        let _ = self.command_tx.send(EngineCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn worker_loop(
    command_rx: Receiver<EngineCommand>,
    preview_rx: Receiver<PreviewRequest>,
    event_tx: Sender<EngineEvent>,
    cancel_export: Arc<AtomicBool>,
) {
    let mut engine = match VipsEngine::new() {
        Ok(engine) => engine,
        Err(error) => {
            error!(%error, "failed to initialize libvips");
            let _ = event_tx.send(EngineEvent::Fatal(error));
            return;
        }
    };

    loop {
        select! {
            recv(command_rx) -> command => match command {
                Ok(EngineCommand::Inspect { path }) => {
                    let result = engine.inspect(&path);
                    let _ = event_tx.send(EngineEvent::Inspected { path, result });
                }
                Ok(EngineCommand::Export(request)) => {
                    let destination = request.destination_path.clone();
                    let _ = event_tx.send(EngineEvent::ExportStarted {
                        destination: destination.clone(),
                    });
                    let result = engine.export(&request, &cancel_export);
                    let _ = event_tx.send(EngineEvent::ExportFinished { destination, result });
                }
                Ok(EngineCommand::CopyClipboard(request)) => {
                    let result = engine.copy_clipboard(&request, &cancel_export);
                    let _ = event_tx.send(EngineEvent::ClipboardReady(result));
                }
                Ok(EngineCommand::Shutdown) | Err(_) => break,
            },
            recv(preview_rx) -> request => match request {
                Ok(request) => {
                    debug!(generation = request.generation, "rendering preview");
                    let result = engine.render_preview(&request);
                    let _ = event_tx.send(EngineEvent::PreviewReady(result));
                }
                Err(_) => break,
            }
        }
    }
}

struct VipsEngine {
    app: VipsApp,
    srgb_profile: PathBuf,
    fit_source: Option<FitSource>,
    fit_adjustment_cache: Option<FitAdjustmentCache>,
    preview_pipeline: Option<PreviewPipeline>,
    #[cfg(test)]
    fit_source_builds: usize,
    #[cfg(test)]
    fit_adjustment_builds: usize,
}

struct FitSource {
    source_path: PathBuf,
    output_width: u32,
    output_height: u32,
    image: VipsImage,
    source_scale: f32,
}

struct PreviewPipeline {
    source_path: PathBuf,
    operations: Vec<Operation>,
    image: VipsImage,
}

struct FitAdjustmentCache {
    source_path: PathBuf,
    output_width: u32,
    output_height: u32,
    operations: Vec<Operation>,
    image: VipsImage,
}

impl VipsEngine {
    fn new() -> Result<Self, RenderError> {
        let app = VipsApp::new("Focusless Edit", false).map_err(vips_error)?;
        let concurrency = thread::available_parallelism()
            .map_or(2, |value| value.get())
            .clamp(1, i32::MAX as usize) as i32;
        app.concurrency_set(concurrency);
        app.cache_set_max(256);
        app.cache_set_max_mem(512 * 1024 * 1024);
        app.cache_set_max_files(64);
        let srgb_profile = resolve_srgb_profile()?;
        Ok(Self {
            app,
            srgb_profile,
            fit_source: None,
            fit_adjustment_cache: None,
            preview_pipeline: None,
            #[cfg(test)]
            fit_source_builds: 0,
            #[cfg(test)]
            fit_adjustment_builds: 0,
        })
    }

    fn inspect(&mut self, path: &Path) -> Result<ImageInfo, RenderError> {
        self.fit_source = None;
        self.fit_adjustment_cache = None;
        self.preview_pipeline = None;
        let source = load_oriented(path)?;
        dimensions(&source)
    }

    fn render_preview(&mut self, request: &PreviewRequest) -> Result<RenderResult, RenderError> {
        validate_viewport(request.viewport)?;
        if request.viewport.zoom <= 0.0 {
            return self.render_source_proxy_preview(request);
        }
        // A restored project stores its effective fit zoom rather than the
        // `0.0` fit sentinel. Materialize the fit source before choosing the
        // preview path so reopening a project does not accidentally send
        // every adjustment through the full-resolution pipeline.
        if self.fit_source.is_none() {
            self.materialize_fit_source(request)?;
        }
        if self.fit_source_supports(request) {
            return self.render_source_proxy_preview(request);
        }
        let base_operations = request
            .operations
            .iter()
            .copied()
            .filter(|operation| !matches!(operation, Operation::Frame { .. }))
            .collect::<Vec<_>>();
        let rebuild_pipeline = self.preview_pipeline.as_ref().is_none_or(|pipeline| {
            pipeline.source_path != request.source_path || pipeline.operations != base_operations
        });
        if rebuild_pipeline {
            let source = load_oriented(&request.source_path)?;
            let source = to_working_linear(&source, &self.srgb_profile)?;
            let image = apply_operations(&source, &base_operations)?;
            self.preview_pipeline = Some(PreviewPipeline {
                source_path: request.source_path.clone(),
                operations: base_operations,
                image,
            });
            debug!(
                generation = request.generation,
                "rebuilt preview operation pipeline"
            );
        } else {
            debug!(
                generation = request.generation,
                "reusing preview operation pipeline"
            );
        }
        let (frame_width_pct, frame_color) = latest_frame(&request.operations);
        let base = &self
            .preview_pipeline
            .as_ref()
            .expect("preview pipeline was initialized")
            .image;
        let adjusted = if frame_width_pct > f32::EPSILON {
            apply_frame_linear(base, frame_width_pct, frame_color)?
        } else {
            base.clone()
        };
        let (visible, effective_zoom) = render_viewport(&adjusted, request.viewport)?;
        finalize_preview(request, visible, effective_zoom)
    }

    fn fit_source_supports(&self, request: &PreviewRequest) -> bool {
        self.fit_source.as_ref().is_some_and(|source| {
            source.source_path == request.source_path
                && source.output_width == request.viewport.output_width
                && source.output_height == request.viewport.output_height
                && request.viewport.zoom <= source.source_scale
        })
    }

    fn render_source_proxy_preview(
        &mut self,
        request: &PreviewRequest,
    ) -> Result<RenderResult, RenderError> {
        self.materialize_fit_source(request)?;
        let source = self
            .fit_source
            .as_ref()
            .expect("fit-preview source was initialized");
        let source_image = source.image.clone();
        let source_scale = source.source_scale;
        let adjusted = if shadows_highlights_active(&request.operations) {
            let cached_operations = operations_through_shadows_highlights(&request.operations);
            let rebuild_cache = self.fit_adjustment_cache.as_ref().is_none_or(|cache| {
                cache.source_path != request.source_path
                    || cache.output_width != request.viewport.output_width
                    || cache.output_height != request.viewport.output_height
                    || cache.operations != cached_operations
            });
            if rebuild_cache {
                let image = apply_operations_through_shadows_highlights_scaled(
                    &source_image,
                    &request.operations,
                )?;
                let image = image.copy_memory().map_err(vips_error)?;
                self.fit_adjustment_cache = Some(FitAdjustmentCache {
                    source_path: request.source_path.clone(),
                    output_width: request.viewport.output_width,
                    output_height: request.viewport.output_height,
                    operations: cached_operations,
                    image,
                });
                #[cfg(test)]
                {
                    self.fit_adjustment_builds += 1;
                }
                debug!(
                    generation = request.generation,
                    "materialized shadows/highlights fit-preview stage"
                );
            } else {
                debug!(
                    generation = request.generation,
                    "reusing shadows/highlights fit-preview stage"
                );
            }
            let cached = self
                .fit_adjustment_cache
                .as_ref()
                .expect("fit adjustment cache was initialized");
            apply_operations_after_shadows_highlights_scaled(
                cached.image.clone(),
                &request.operations,
                source_scale,
            )?
        } else {
            self.fit_adjustment_cache = None;
            apply_operations_scaled(&source_image, &request.operations, source_scale)?
        };
        let proxy_viewport = Viewport {
            zoom: if request.viewport.zoom <= 0.0 {
                0.0
            } else {
                request.viewport.zoom / source.source_scale
            },
            ..request.viewport
        };
        let (visible, proxy_zoom) = render_viewport(&adjusted, proxy_viewport)?;
        finalize_preview(request, visible, source.source_scale * proxy_zoom)
    }

    fn materialize_fit_source(&mut self, request: &PreviewRequest) -> Result<(), RenderError> {
        let rebuild_source = self.fit_source.as_ref().is_none_or(|source| {
            source.source_path != request.source_path
                || source.output_width != request.viewport.output_width
                || source.output_height != request.viewport.output_height
        });
        if rebuild_source {
            self.fit_adjustment_cache = None;
            let source = load_oriented(&request.source_path)?;
            let source = to_working_linear(&source, &self.srgb_profile)?;
            let info = dimensions(&source)?;
            let fit_scale = (f64::from(request.viewport.output_width) / f64::from(info.width))
                .min(f64::from(request.viewport.output_height) / f64::from(info.height));
            let source_scale = (fit_scale * 1.5).min(1.0);
            let proxy = if (source_scale - 1.0).abs() < f64::EPSILON {
                source
            } else {
                ops::resize(&source, source_scale).map_err(vips_error)?
            };
            let proxy = proxy.copy_memory().map_err(vips_error)?;
            self.fit_source = Some(FitSource {
                source_path: request.source_path.clone(),
                output_width: request.viewport.output_width,
                output_height: request.viewport.output_height,
                image: proxy,
                source_scale: source_scale as f32,
            });
            #[cfg(test)]
            {
                self.fit_source_builds += 1;
            }
            debug!(
                generation = request.generation,
                "materialized color-managed fit-preview source"
            );
        }
        Ok(())
    }
}

fn finalize_preview(
    request: &PreviewRequest,
    visible: VipsImage,
    effective_zoom: f32,
) -> Result<RenderResult, RenderError> {
    let display = from_working_linear(&visible)?;
    let rgba = ensure_rgba8(&display)?;
    let canvas = fit_to_canvas(&rgba, request.viewport)?;
    let info = dimensions(&canvas)?;
    let rgba8 = canvas.write_to_memory();
    let expected_len = info.width as usize * info.height as usize * 4;
    if rgba8.len() != expected_len {
        return Err(RenderError::Engine(format!(
            "libvips returned {} preview bytes; expected {expected_len}",
            rgba8.len()
        )));
    }

    Ok(RenderResult {
        generation: request.generation,
        width: info.width,
        height: info.height,
        rgba8,
        effective_zoom,
    })
}

impl VipsEngine {
    fn export(&self, request: &ExportRequest, cancelled: &AtomicBool) -> Result<(), RenderError> {
        if cancelled.load(Ordering::Acquire) {
            return Err(RenderError::Cancelled);
        }

        let source = load_oriented(&request.source_path)?;
        let source = to_working_linear(&source, &self.srgb_profile)?;
        let adjusted = apply_operations(&source, &request.operations)?;
        let adjusted = from_working_linear(&adjusted)?;
        let temporary = export_temporary_path(&request.destination_path);
        if temporary.exists() {
            fs::remove_file(&temporary)?;
        }
        let temporary_str = path_string(&temporary)?;
        let profile = profile_string(&self.srgb_profile)?;

        let result = match request.format {
            ExportFormat::Jpeg { quality } => {
                let flattened = flatten_for_jpeg(&adjusted)?;
                flattened.write_to_file(format!(
                    "{temporary_str}[Q={},optimize-coding=true,interlace=true,keep=none,profile={profile}]",
                    quality.clamp(1, 100)
                ))
            }
            ExportFormat::Png => adjusted.write_to_file(format!(
                "{temporary_str}[compression=6,keep=none,profile={profile}]"
            )),
            // Filename options let each installed libvips version negotiate
            // the supported WebP saver surface itself.
            ExportFormat::WebP { quality } => adjusted.write_to_file(format!(
                "{temporary_str}[Q={},keep=none,profile={profile}]",
                quality.clamp(1, 100),
            )),
        };
        if let Err(error) = result {
            let detail = self
                .app
                .error_buffer()
                .unwrap_or_else(|_| "no libvips details".into());
            let message = format!("{error}: {}", detail.trim());
            self.app.error_clear();
            let _ = fs::remove_file(&temporary);
            return Err(RenderError::Engine(message));
        }

        if cancelled.load(Ordering::Acquire) {
            let _ = fs::remove_file(&temporary);
            return Err(RenderError::Cancelled);
        }
        replace_file(&temporary, &request.destination_path)?;
        Ok(())
    }

    fn copy_clipboard(
        &self,
        request: &CopyRequest,
        cancelled: &AtomicBool,
    ) -> Result<CopyResult, RenderError> {
        if cancelled.load(Ordering::Acquire) {
            return Err(RenderError::Cancelled);
        }

        let source = load_oriented(&request.source_path)?;
        let source = to_working_linear(&source, &self.srgb_profile)?;
        let adjusted = apply_operations(&source, &request.operations)?;
        let display = from_working_linear(&adjusted)?;
        let rgba = ensure_rgba8(&display)?;
        let info = dimensions(&rgba)?;
        let rgba8 = rgba.write_to_memory();

        Ok(CopyResult {
            width: info.width,
            height: info.height,
            rgba8,
        })
    }

    #[allow(dead_code)]
    fn version(&self) -> String {
        self.app
            .version_string()
            .unwrap_or_else(|_| "unknown".into())
    }
}

fn load_oriented(path: &Path) -> Result<VipsImage, RenderError> {
    let path = path_string(path)?;
    let source = VipsImage::new_from_file(&path).map_err(vips_error)?;
    ops::autorot(&source).map_err(vips_error)
}

fn resolve_srgb_profile() -> Result<PathBuf, RenderError> {
    srgb_profile_candidates()
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            RenderError::Engine(
                "an sRGB ICC profile is required; set FOCUSLESS_SRGB_PROFILE or provide the \
                 standard sRGB profile for this operating system"
                    .into(),
            )
        })
}

fn srgb_profile_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = env::var_os("FOCUSLESS_SRGB_PROFILE") {
        candidates.push(PathBuf::from(path));
    }
    if let Some(executable_directory) = env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(Path::to_path_buf))
    {
        candidates.push(executable_directory.join("sRGB Color Space Profile.icm"));
        candidates.push(executable_directory.join("sRGB.icc"));
    }
    candidates.extend(platform_srgb_profile_candidates());
    candidates
}

#[cfg(target_os = "windows")]
fn platform_srgb_profile_candidates() -> Vec<PathBuf> {
    env::var_os("SystemRoot")
        .map(PathBuf::from)
        .into_iter()
        .map(|root| {
            root.join("System32")
                .join("spool")
                .join("drivers")
                .join("color")
                .join("sRGB Color Space Profile.icm")
        })
        .collect()
}

#[cfg(not(target_os = "windows"))]
fn platform_srgb_profile_candidates() -> Vec<PathBuf> {
    [
        "/usr/share/color/icc/sRGB.icc",
        "/usr/share/color/icc/ghostscript/srgb.icc",
        "/usr/share/color/icc/colord/sRGB.icc",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

fn profile_string(path: &Path) -> Result<String, RenderError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| RenderError::Engine("sRGB ICC profile path is not valid UTF-8".into()))
}

fn to_working_linear(image: &VipsImage, srgb_profile: &Path) -> Result<VipsImage, RenderError> {
    let has_alpha = image.hasalpha();
    let bands = image.get_bands();
    let color_bands = if has_alpha { bands - 1 } else { bands };
    let color = if has_alpha {
        ops::extract_band_with_opts(image, 0, &ops::ExtractBandOptions { n: color_bands })
            .map_err(vips_error)?
    } else {
        ops::copy(image).map_err(vips_error)?
    };
    let alpha = if has_alpha {
        let alpha = ops::extract_band(image, bands - 1).map_err(vips_error)?;
        Some(alpha_to_unit(
            &alpha,
            image.get_format().map_err(vips_error)?,
        )?)
    } else {
        None
    };

    let color = if color_bands == 1 {
        ops::colourspace(&color, ops::Interpretation::Srgb).map_err(vips_error)?
    } else {
        color
    };
    let profile = profile_string(srgb_profile)?;
    let managed_srgb = ops::icc_transform_with_opts(
        &color,
        &profile,
        &ops::IccTransformOptions {
            intent: ops::Intent::Relative,
            black_point_compensation: true,
            embedded: true,
            input_profile: Some(profile.clone()),
            depth: 16,
        },
    )
    .map_err(vips_error)?;
    let linear = ops::s_rgb2sc_rgb(&managed_srgb).map_err(vips_error)?;
    if let Some(alpha) = alpha {
        ops::bandjoin(&mut [linear, alpha]).map_err(vips_error)
    } else {
        Ok(linear)
    }
}

fn from_working_linear(image: &VipsImage) -> Result<VipsImage, RenderError> {
    let has_alpha = image.hasalpha();
    let bands = image.get_bands();
    let color = if has_alpha {
        ops::extract_band_with_opts(image, 0, &ops::ExtractBandOptions { n: bands - 1 })
            .map_err(vips_error)?
    } else {
        ops::copy(image).map_err(vips_error)?
    };
    let srgb = ops::sc_rgb2s_rgb(&color).map_err(vips_error)?;
    if has_alpha {
        let alpha = ops::extract_band(image, bands - 1).map_err(vips_error)?;
        let alpha = ops::linear(&alpha, &mut [255.0], &mut [0.0]).map_err(vips_error)?;
        let alpha = ops::cast(&alpha, ops::BandFormat::Uchar).map_err(vips_error)?;
        ops::bandjoin(&mut [srgb, alpha]).map_err(vips_error)
    } else {
        Ok(srgb)
    }
}

fn flatten_for_jpeg(image: &VipsImage) -> Result<VipsImage, RenderError> {
    if !image.hasalpha() {
        return ops::copy(image).map_err(vips_error);
    }
    let color_bands = (image.get_bands() - 1).max(1) as usize;
    ops::flatten_with_opts(
        image,
        &ops::FlattenOptions {
            background: vec![255.0; color_bands],
        },
    )
    .map_err(vips_error)
}

fn alpha_to_unit(
    alpha: &VipsImage,
    source_format: ops::BandFormat,
) -> Result<VipsImage, RenderError> {
    let maximum = match source_format {
        ops::BandFormat::Uchar => 255.0,
        ops::BandFormat::Ushort => 65_535.0,
        ops::BandFormat::Uint => u32::MAX as f64,
        ops::BandFormat::Char => i8::MAX as f64,
        ops::BandFormat::Short => i16::MAX as f64,
        ops::BandFormat::Int => i32::MAX as f64,
        ops::BandFormat::Float | ops::BandFormat::Double => 1.0,
        format => {
            return Err(RenderError::Engine(format!(
                "unsupported alpha channel format: {format:?}"
            )));
        }
    };
    let alpha = ops::cast(alpha, ops::BandFormat::Float).map_err(vips_error)?;
    ops::linear(&alpha, &mut [1.0 / maximum], &mut [0.0]).map_err(vips_error)
}

fn apply_operations(image: &VipsImage, operations: &[Operation]) -> Result<VipsImage, RenderError> {
    apply_operations_scaled(image, operations, 1.0)
}

fn apply_operations_scaled(
    image: &VipsImage,
    operations: &[Operation],
    source_scale: f32,
) -> Result<VipsImage, RenderError> {
    let current = apply_operations_through_shadows_highlights_scaled(image, operations)?;
    apply_operations_after_shadows_highlights_scaled(current, operations, source_scale)
}

fn apply_operations_through_shadows_highlights_scaled(
    image: &VipsImage,
    operations: &[Operation],
) -> Result<VipsImage, RenderError> {
    let mut current = ops::copy(image).map_err(vips_error)?;

    let quarter_turns = operations
        .iter()
        .rev()
        .find_map(|operation| match *operation {
            Operation::Rotate { quarter_turns } => Some(quarter_turns % 4),
            _ => None,
        })
        .unwrap_or(0);
    current = match quarter_turns {
        0 => current,
        1 => ops::rot(&current, ops::Angle::D90).map_err(vips_error)?,
        2 => ops::rot(&current, ops::Angle::D180).map_err(vips_error)?,
        3 => ops::rot(&current, ops::Angle::D270).map_err(vips_error)?,
        _ => unreachable!(),
    };

    let straighten = operations
        .iter()
        .rev()
        .find_map(|operation| match *operation {
            Operation::Straighten { degrees } => Some(degrees),
            _ => None,
        })
        .unwrap_or(0.0);
    if straighten.abs() > f32::EPSILON {
        current = apply_straighten_linear(&current, straighten)?;
    }

    let crop = operations
        .iter()
        .rev()
        .find_map(|operation| match *operation {
            Operation::Crop { rect } => Some(rect),
            _ => None,
        })
        .unwrap_or(CropRect::FULL);
    if !crop.is_full() {
        current = crop_image(&current, crop)?;
    }

    let white_balance = operations
        .iter()
        .rev()
        .find_map(|operation| match *operation {
            Operation::WhiteBalance { adjustment } => Some(adjustment),
            _ => None,
        })
        .unwrap_or_default();
    if !white_balance.is_identity() {
        current = apply_white_balance_linear(&current, white_balance)?;
    }

    let exposure_ev = operations
        .iter()
        .rev()
        .find_map(|operation| match *operation {
            Operation::Exposure { ev } => Some(ev),
            _ => None,
        })
        .unwrap_or(0.0);
    if exposure_ev.abs() > f32::EPSILON {
        current = apply_exposure_linear(&current, exposure_ev)?;
    }
    let shadows_highlights = operations
        .iter()
        .rev()
        .find_map(|operation| match *operation {
            Operation::ShadowsHighlights { adjustment } => Some(adjustment),
            _ => None,
        })
        .unwrap_or_default();
    if !shadows_highlights.is_identity() {
        current = apply_shadows_highlights_guided(&current, shadows_highlights)?;
    }

    Ok(current)
}

fn apply_operations_after_shadows_highlights_scaled(
    mut current: VipsImage,
    operations: &[Operation],
    source_scale: f32,
) -> Result<VipsImage, RenderError> {
    let contrast = operations
        .iter()
        .rev()
        .find_map(|operation| match *operation {
            Operation::Contrast { amount } => Some(amount),
            _ => None,
        })
        .unwrap_or(0.0);
    if contrast.abs() > f32::EPSILON {
        current = apply_contrast_oklab(&current, contrast)?;
    }
    let tone_curve = operations
        .iter()
        .rev()
        .find_map(|operation| match *operation {
            Operation::ToneCurve { curve } => Some(curve),
            _ => None,
        })
        .unwrap_or_default();
    if !tone_curve.is_identity() {
        current = apply_tone_curve_linear(&current, tone_curve)?;
    }

    // Saturation, Vignette and Sharpness all operate on OKLab lightness (and
    // chroma for Saturation). We convert to OKLab once, run all three in that
    // space, then convert back to linear scRGB once at the end — avoiding two
    // redundant round-trips compared to running each step independently.
    let saturation = operations
        .iter()
        .rev()
        .find_map(|operation| match *operation {
            Operation::Saturation { amount } => Some(amount),
            _ => None,
        })
        .unwrap_or(0.0);
    let vignette_strength = operations
        .iter()
        .rev()
        .find_map(|operation| match *operation {
            Operation::Vignette { strength } => Some(strength),
            _ => None,
        })
        .unwrap_or(0.0);
    let sharpness = operations
        .iter()
        .rev()
        .find_map(|operation| match *operation {
            Operation::Sharpness { amount } => Some(amount),
            _ => None,
        })
        .unwrap_or(0.0);

    let needs_oklab_pass = saturation.abs() > f32::EPSILON
        || vignette_strength > f32::EPSILON
        || sharpness > f32::EPSILON;

    if needs_oklab_pass {
        // Split color and alpha once for the entire OKLab pass.
        let (color, alpha) = split_color_and_alpha(&current, "tonal adjustments")?;
        let mut oklab = linear_rgb_to_oklab(&color)?;

        // 1. Saturation: scale the a and b chroma axes.
        if saturation.abs() > f32::EPSILON {
            let chroma_scale = 1.0 + f64::from(saturation) / 100.0;
            oklab = ops::linear(
                &oklab,
                &mut [1.0, chroma_scale, chroma_scale],
                &mut [0.0, 0.0, 0.0],
            )
            .map_err(vips_error)?;
        }

        // 2. Vignette: darken OKLab L proportionally to radial distance.
        if vignette_strength > f32::EPSILON {
            let info = dimensions(&current)?;
            oklab = apply_vignette_on_oklab(&oklab, vignette_strength, info.width, info.height)?;
        }

        // 3. Sharpness: unsharp-mask on OKLab L only.
        if sharpness > f32::EPSILON {
            oklab = apply_sharpness_on_oklab(&oklab, sharpness, source_scale)?;
        }

        // Single conversion back to linear scRGB.
        let adjusted_color = oklab_to_linear_rgb(&oklab)?;
        current = join_alpha(adjusted_color, alpha)?;
    }

    let (frame_width_pct, frame_color) = latest_frame(operations);
    if frame_width_pct > f32::EPSILON {
        current = apply_frame_linear(&current, frame_width_pct, frame_color)?;
    }

    Ok(current)
}

fn shadows_highlights_active(operations: &[Operation]) -> bool {
    operations
        .iter()
        .rev()
        .find_map(|operation| match *operation {
            Operation::ShadowsHighlights { adjustment } => Some(adjustment),
            _ => None,
        })
        .is_some_and(|adjustment| !adjustment.is_identity())
}

fn operations_through_shadows_highlights(operations: &[Operation]) -> Vec<Operation> {
    operations
        .iter()
        .copied()
        .filter(|operation| {
            matches!(
                operation,
                Operation::Rotate { .. }
                    | Operation::Straighten { .. }
                    | Operation::Crop { .. }
                    | Operation::WhiteBalance { .. }
                    | Operation::Exposure { .. }
                    | Operation::Contrast { .. }
                    | Operation::ShadowsHighlights { .. }
            )
        })
        .collect()
}

fn latest_frame(operations: &[Operation]) -> (f32, FrameColor) {
    operations
        .iter()
        .rev()
        .find_map(|operation| match *operation {
            Operation::Frame { width_pct, color } => Some((width_pct, color)),
            _ => None,
        })
        .unwrap_or((0.0, FrameColor::WHITE))
}

fn apply_straighten_linear(image: &VipsImage, degrees: f32) -> Result<VipsImage, RenderError> {
    let source = dimensions(image)?;
    let with_alpha = if image.hasalpha() {
        ops::copy(image).map_err(vips_error)?
    } else {
        ops::addalpha(image).map_err(vips_error)?
    };
    let premultiplied = ops::premultiply(&with_alpha).map_err(vips_error)?;
    let rotated = ops::rotate(&premultiplied, f64::from(degrees)).map_err(vips_error)?;
    let rotated = ops::unpremultiply(&rotated).map_err(vips_error)?;
    let radians = f64::from(degrees.abs()).to_radians();
    let (crop_width, crop_height) = largest_rotated_rect(
        f64::from(source.width),
        f64::from(source.height),
        radians.sin(),
        radians.cos(),
    );
    let rotated_info = dimensions(&rotated)?;
    let crop_width = (crop_width.round() as u32)
        .clamp(1, rotated_info.width)
        .min(i32::MAX as u32);
    let crop_height = (crop_height.round() as u32)
        .clamp(1, rotated_info.height)
        .min(i32::MAX as u32);
    let left = ((rotated_info.width - crop_width) / 2) as i32;
    let top = ((rotated_info.height - crop_height) / 2) as i32;
    ops::extract_area(&rotated, left, top, crop_width as i32, crop_height as i32)
        .map_err(vips_error)
}

fn largest_rotated_rect(width: f64, height: f64, sine: f64, cosine: f64) -> (f64, f64) {
    let width_is_longer = width >= height;
    let (long_side, short_side) = if width_is_longer {
        (width, height)
    } else {
        (height, width)
    };
    let (cropped_long, cropped_short) =
        if short_side <= 2.0 * sine * cosine * long_side || (sine - cosine).abs() < f64::EPSILON {
            let half_short = 0.5 * short_side;
            (
                half_short / sine.max(f64::EPSILON),
                half_short / cosine.max(f64::EPSILON),
            )
        } else {
            let cosine_double = cosine * cosine - sine * sine;
            (
                (long_side * cosine - short_side * sine) / cosine_double,
                (short_side * cosine - long_side * sine) / cosine_double,
            )
        };
    if width_is_longer {
        (cropped_long, cropped_short)
    } else {
        (cropped_short, cropped_long)
    }
}

fn apply_frame_linear(
    image: &VipsImage,
    width_pct: f32,
    color: FrameColor,
) -> Result<VipsImage, RenderError> {
    let width = image.get_width();
    let height = image.get_height();
    let border_px = ((width.min(height) as f32 * width_pct / 100.0).round() as i32).max(1);
    embed_frame_linear(
        image,
        border_px,
        border_px,
        width + 2 * border_px,
        height + 2 * border_px,
        color,
    )
}

fn embed_frame_linear(
    image: &VipsImage,
    left: i32,
    top: i32,
    width: i32,
    height: i32,
    color: FrameColor,
) -> Result<VipsImage, RenderError> {
    fn srgb_to_linear(v: u8) -> f64 {
        let v = f64::from(v) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }

    let mut background = vec![
        srgb_to_linear(color.r),
        srgb_to_linear(color.g),
        srgb_to_linear(color.b),
    ];
    if image.hasalpha() {
        background.push(1.0);
    }

    ops::embed_with_opts(
        image,
        left,
        top,
        width,
        height,
        &ops::EmbedOptions {
            extend: ops::Extend::Background,
            background,
        },
    )
    .map_err(vips_error)
}

fn apply_white_balance_linear(
    image: &VipsImage,
    adjustment: WhiteBalance,
) -> Result<VipsImage, RenderError> {
    let bands = image.get_bands();
    let has_alpha = image.hasalpha();
    let color = if has_alpha {
        ops::extract_band_with_opts(image, 0, &ops::ExtractBandOptions { n: bands - 1 })
            .map_err(vips_error)?
    } else {
        ops::copy(image).map_err(vips_error)?
    };
    if color.get_bands() != 3 {
        return Err(RenderError::Engine(format!(
            "white balance requires three color bands, got {}",
            color.get_bands()
        )));
    }
    let alpha = if has_alpha {
        Some(ops::extract_band(image, bands - 1).map_err(vips_error)?)
    } else {
        None
    };

    let balanced = recombine_three_channels(&color, white_balance_rgb_matrix(adjustment)?)?;
    if let Some(alpha) = alpha {
        ops::bandjoin(&mut [balanced, alpha]).map_err(vips_error)
    } else {
        Ok(balanced)
    }
}

const LINEAR_RGB_TO_LMS: [[f32; 3]; 3] = [
    [0.412_221_46, 0.536_332_55, 0.051_445_995],
    [0.211_903_5, 0.680_699_5, 0.107_396_96],
    [0.088_302_46, 0.281_718_85, 0.629_978_7],
];
const LMS_ROOT_TO_OKLAB: [[f32; 3]; 3] = [
    [0.210_454_26, 0.793_617_8, -0.004_072_047],
    [1.977_998_5, -2.428_592_2, 0.450_593_7],
    [0.025_904_037, 0.782_771_77, -0.808_675_77],
];
const OKLAB_TO_LMS_ROOT: [[f32; 3]; 3] = [
    [1.0, 0.396_337_78, 0.215_803_76],
    [1.0, -0.105_561_346, -0.063_854_17],
    [1.0, -0.089_484_18, -1.291_485_5],
];
const LMS_TO_LINEAR_RGB: [[f32; 3]; 3] = [
    [4.076_741_7, -3.307_711_6, 0.230_969_94],
    [-1.268_438, 2.609_757_4, -0.341_319_4],
    [-0.004_196_086_3, -0.703_418_6, 1.707_614_7],
];

/// Darken the OKLab lightness channel with a radially symmetric vignette.
///
/// The alpha mask is computed entirely with libvips tensor operations — no
/// pixel loops. The result is in OKLab space so the next stage (Sharpness)
/// can consume it directly without an extra color-space round-trip.
///
/// Grid convention: the exact image center maps to (0, 0); coordinate values
/// are normalized by half the shorter dimension so a circle inscribed in
/// the shorter axis reaches radius 1.0 regardless of aspect ratio.
fn apply_vignette_on_oklab(
    oklab: &VipsImage,
    strength: f32,
    width: u32,
    height: u32,
) -> Result<VipsImage, RenderError> {
    let s = f64::from(strength);
    let w = width as i32;
    let h = height as i32;
    let n = (w * h) as usize;

    // --- Build normalized coordinate images in one pass ---------------------
    // We fill two float buffers (u, v) and hand them to libvips as single-band
    // float images. All subsequent math is libvips tensor ops — fully parallel.
    let half_short = f64::from(w.min(h)) * 0.5;
    let cx = f64::from(w - 1) * 0.5;
    let cy = f64::from(h - 1) * 0.5;

    let mut u_bytes = Vec::with_capacity(n * size_of::<f32>());
    let mut v_bytes = Vec::with_capacity(n * size_of::<f32>());
    for row in 0..h {
        let v_val = (f64::from(row) - cy) / half_short;
        for col in 0..w {
            let u_val = (f64::from(col) - cx) / half_short;
            u_bytes.extend_from_slice(&(u_val as f32).to_ne_bytes());
            v_bytes.extend_from_slice(&(v_val as f32).to_ne_bytes());
        }
    }

    let u = VipsImage::new_from_memory_copy(&u_bytes, w, h, 1, ops::BandFormat::Float)
        .map_err(vips_error)?;
    let v = VipsImage::new_from_memory_copy(&v_bytes, w, h, 1, ops::BandFormat::Float)
        .map_err(vips_error)?;

    // --- Radial distance r = sqrt(u^2 + v^2) --------------------------------
    let u2 = ops::multiply(&u, &u).map_err(vips_error)?;
    let v2 = ops::multiply(&v, &v).map_err(vips_error)?;
    let r2 = ops::add(&u2, &v2).map_err(vips_error)?;
    // sqrt via r2^0.5
    let r = ops::math2_const(&r2, ops::OperationMath2::Pow, &mut [0.5]).map_err(vips_error)?;

    // --- Smoothstep falloff -------------------------------------------------
    // edge0 = 1.0 - 0.6 * s  (inner boundary, moves inward as strength grows)
    // edge1 = 1.4             (outer boundary, constant)
    // t     = clamp((r - edge0) / (edge1 - edge0), 0, 1)
    // alpha_base = t^2 * (3 - 2*t)   (smoothstep)
    // alpha      = s * alpha_base
    let edge0 = 1.0 - 0.6 * s;
    let edge1 = 1.4_f64;
    let ramp_scale = 1.0 / (edge1 - edge0);
    let ramp_offset = -edge0 / (edge1 - edge0);
    // t (un-clamped)
    let t_raw = ops::linear(&r, &mut [ramp_scale], &mut [ramp_offset]).map_err(vips_error)?;
    // clamp to [0, 1]
    let t = clamp_image(&t_raw, 0.0, 1.0)?;
    // smoothstep: t^2 * (3 - 2*t)
    let t2 = ops::multiply(&t, &t).map_err(vips_error)?;
    let three_minus_2t = ops::linear(&t, &mut [-2.0], &mut [3.0]).map_err(vips_error)?;
    let alpha_base = ops::multiply(&t2, &three_minus_2t).map_err(vips_error)?;
    // Scale by strength: alpha = s * alpha_base
    let alpha_mask = ops::linear(&alpha_base, &mut [s], &mut [0.0]).map_err(vips_error)?;

    // --- Apply to L channel only --------------------------------------------
    // L_out = L_in * (1.0 - alpha)
    let one_minus_alpha = ops::linear(&alpha_mask, &mut [-1.0], &mut [1.0]).map_err(vips_error)?;
    let lightness = ops::extract_band(oklab, 0).map_err(vips_error)?;
    let chroma = ops::extract_band_with_opts(oklab, 1, &ops::ExtractBandOptions { n: 2 })
        .map_err(vips_error)?;
    let darkened_lightness = ops::multiply(&lightness, &one_minus_alpha).map_err(vips_error)?;

    // Re-join L' + a + b and return OKLab (no scRGB conversion).
    ops::bandjoin(&mut [darkened_lightness, chroma]).map_err(vips_error)
}

/// Apply unsharp-mask sharpening directly on an OKLab image.
///
/// Identical math to `apply_sharpness_oklab` but accepts an already-converted
/// OKLab tensor and returns one, avoiding the redundant forward conversion.
fn apply_sharpness_on_oklab(
    oklab: &VipsImage,
    amount: f32,
    source_scale: f32,
) -> Result<VipsImage, RenderError> {
    const RADIUS_SIGMA: f64 = 1.0;
    const DETAIL_THRESHOLD: f64 = 0.003;

    let lightness = ops::extract_band(oklab, 0).map_err(vips_error)?;
    let chroma = ops::extract_band_with_opts(oklab, 1, &ops::ExtractBandOptions { n: 2 })
        .map_err(vips_error)?;
    let radius_sigma = (RADIUS_SIGMA * f64::from(source_scale)).max(0.2);
    let blurred = ops::gaussblur(&lightness, radius_sigma).map_err(vips_error)?;
    let detail = ops::subtract(&lightness, &blurred).map_err(vips_error)?;
    let detail_magnitude = ops::abs(&detail).map_err(vips_error)?;
    let significant = ops::relational_const(
        &detail_magnitude,
        ops::OperationRelational::Moreeq,
        &mut [DETAIL_THRESHOLD],
    )
    .map_err(vips_error)?;
    let scaled_detail =
        ops::linear(&detail, &mut [f64::from(amount) / 100.0], &mut [0.0]).map_err(vips_error)?;
    let sharpened_lightness = ops::add(&lightness, &scaled_detail).map_err(vips_error)?;
    let adjusted_lightness =
        ops::ifthenelse(&significant, &sharpened_lightness, &lightness).map_err(vips_error)?;
    ops::bandjoin(&mut [adjusted_lightness, chroma]).map_err(vips_error)
}

fn apply_contrast_oklab(image: &VipsImage, amount: f32) -> Result<VipsImage, RenderError> {
    let (color, alpha) = split_color_and_alpha(image, "contrast")?;
    let oklab = linear_rgb_to_oklab(&color)?;

    let lightness = ops::extract_band(&oklab, 0).map_err(vips_error)?;
    let chroma = ops::extract_band_with_opts(&oklab, 1, &ops::ExtractBandOptions { n: 2 })
        .map_err(vips_error)?;

    let gamma = 1.0 + f64::from(amount) / 100.0;

    let condition = ops::relational_const(&lightness, ops::OperationRelational::Lesseq, &mut [0.5])
        .map_err(vips_error)?;

    let two_i = ops::linear(&lightness, &mut [2.0], &mut [0.0]).map_err(vips_error)?;
    let two_i_abs = ops::abs(&two_i).map_err(vips_error)?;
    let shadows_pow =
        ops::math2_const(&two_i_abs, ops::OperationMath2::Pow, &mut [gamma]).map_err(vips_error)?;
    let shadows = ops::linear(&shadows_pow, &mut [0.5], &mut [0.0]).map_err(vips_error)?;

    let inverted = ops::linear(&lightness, &mut [-2.0], &mut [2.0]).map_err(vips_error)?;
    let inverted_abs = ops::abs(&inverted).map_err(vips_error)?;
    let highlights_pow = ops::math2_const(&inverted_abs, ops::OperationMath2::Pow, &mut [gamma])
        .map_err(vips_error)?;
    let highlights = ops::linear(&highlights_pow, &mut [-0.5], &mut [1.0]).map_err(vips_error)?;

    let adjusted_lightness =
        ops::ifthenelse(&condition, &shadows, &highlights).map_err(vips_error)?;
    let below = ops::relational_const(&lightness, ops::OperationRelational::Less, &mut [0.0])
        .map_err(vips_error)?;
    let above = ops::relational_const(&lightness, ops::OperationRelational::More, &mut [1.0])
        .map_err(vips_error)?;
    let adjusted_lightness =
        ops::ifthenelse(&below, &lightness, &adjusted_lightness).map_err(vips_error)?;
    let adjusted_lightness =
        ops::ifthenelse(&above, &lightness, &adjusted_lightness).map_err(vips_error)?;

    let adjusted_oklab = ops::bandjoin(&mut [adjusted_lightness, chroma]).map_err(vips_error)?;
    let contrasted = oklab_to_linear_rgb(&adjusted_oklab)?;
    join_alpha(contrasted, alpha)
}

/// Edge-aware shadows/highlights adjustment on OKLab lightness.
///
/// A self-guided Gaussian filter estimates a locally smooth log-lightness
/// base while preserving strong edges. Shadow and highlight masks come from
/// that base, but their shifts are applied to the original lightness so fine
/// detail is retained. This is the fast guided-filter form of local tone
/// mapping: it avoids the halos of a plain blurred mask and the cost of a
/// multi-level Laplacian reconstruction.
fn apply_shadows_highlights_guided(
    image: &VipsImage,
    adjustment: ShadowsHighlights,
) -> Result<VipsImage, RenderError> {
    const EPSILON: f64 = 1e-4;
    const GUIDED_EPSILON: f64 = 0.04;
    const MASK_RANGE: f64 = 2.5;
    const MAX_SHIFT: f64 = 1.0;

    let (color, alpha) = split_color_and_alpha(image, "shadows/highlights")?;
    let oklab = linear_rgb_to_oklab(&color)?;
    let lightness = ops::extract_band(&oklab, 0).map_err(vips_error)?;
    let chroma = ops::extract_band_with_opts(&oklab, 1, &ops::ExtractBandOptions { n: 2 })
        .map_err(vips_error)?;

    // The mask domain is display-referred OKLab L. Extended-range samples are
    // used safely for mask construction, then restored unchanged below.
    let bounded_lightness = clamp_image(&lightness, EPSILON, 1.0)?;
    let log_lightness =
        ops::math(&bounded_lightness, ops::OperationMath::Log).map_err(vips_error)?;

    // Scale the local neighborhood with the current working image, so fitted
    // proxies and full-resolution exports use the same relative radius.
    let info = dimensions(image)?;
    let sigma = (f64::from(info.width.min(info.height)) / 300.0).clamp(1.0, 32.0);
    let mean = ops::gaussblur(&log_lightness, sigma).map_err(vips_error)?;
    let squared = ops::multiply(&log_lightness, &log_lightness).map_err(vips_error)?;
    let correlation = ops::gaussblur(&squared, sigma).map_err(vips_error)?;
    let mean_squared = ops::multiply(&mean, &mean).map_err(vips_error)?;
    let variance = ops::subtract(&correlation, &mean_squared).map_err(vips_error)?;
    let variance = clamp_image(&variance, 0.0, f64::from(f32::MAX))?;
    let denominator =
        ops::linear(&variance, &mut [1.0], &mut [GUIDED_EPSILON]).map_err(vips_error)?;
    let coefficient = ops::divide(&variance, &denominator).map_err(vips_error)?;
    let coefficient_mean = ops::gaussblur(&coefficient, sigma).map_err(vips_error)?;
    let coefficient_times_mean = ops::multiply(&coefficient, &mean).map_err(vips_error)?;
    let intercept = ops::subtract(&mean, &coefficient_times_mean).map_err(vips_error)?;
    let intercept_mean = ops::gaussblur(&intercept, sigma).map_err(vips_error)?;
    let guided_part = ops::multiply(&coefficient_mean, &log_lightness).map_err(vips_error)?;
    let local_base = ops::add(&guided_part, &intercept_mean).map_err(vips_error)?;

    let mid_log = 0.5_f64.ln();
    let shadow_raw = ops::linear(
        &local_base,
        &mut [-1.0 / MASK_RANGE],
        &mut [mid_log / MASK_RANGE],
    )
    .map_err(vips_error)?;
    let highlight_raw = ops::linear(
        &local_base,
        &mut [1.0 / MASK_RANGE],
        &mut [-mid_log / MASK_RANGE],
    )
    .map_err(vips_error)?;
    let shadow_mask = smoothstep_image(&clamp_image(&shadow_raw, 0.0, 1.0)?)?;
    let highlight_mask = smoothstep_image(&clamp_image(&highlight_raw, 0.0, 1.0)?)?;

    let shadow_shift = -f64::from(adjustment.shadows) / 100.0 * MAX_SHIFT;
    let highlight_shift = f64::from(adjustment.highlights) / 100.0 * MAX_SHIFT;
    let shadow_shifted =
        ops::linear(&shadow_mask, &mut [shadow_shift], &mut [0.0]).map_err(vips_error)?;
    let highlight_shifted =
        ops::linear(&highlight_mask, &mut [highlight_shift], &mut [0.0]).map_err(vips_error)?;
    let shift = ops::add(&shadow_shifted, &highlight_shifted).map_err(vips_error)?;
    let adjusted_log = ops::add(&log_lightness, &shift).map_err(vips_error)?;
    let adjusted_lightness =
        ops::math(&adjusted_log, ops::OperationMath::Exp).map_err(vips_error)?;

    // Preserve extended-range lightness exactly. Only the documented 0..1
    // adjustment domain participates in the local tone mapping.
    let below = ops::relational_const(&lightness, ops::OperationRelational::Less, &mut [0.0])
        .map_err(vips_error)?;
    let above = ops::relational_const(&lightness, ops::OperationRelational::More, &mut [1.0])
        .map_err(vips_error)?;
    let adjusted_lightness =
        ops::ifthenelse(&below, &lightness, &adjusted_lightness).map_err(vips_error)?;
    let adjusted_lightness =
        ops::ifthenelse(&above, &lightness, &adjusted_lightness).map_err(vips_error)?;

    let adjusted_oklab = ops::bandjoin(&mut [adjusted_lightness, chroma]).map_err(vips_error)?;
    let adjusted_color = oklab_to_linear_rgb(&adjusted_oklab)?;
    join_alpha(adjusted_color, alpha)
}

fn clamp_image(image: &VipsImage, minimum: f64, maximum: f64) -> Result<VipsImage, RenderError> {
    let minimum_image = ops::linear(image, &mut [0.0], &mut [minimum]).map_err(vips_error)?;
    let maximum_image = ops::linear(image, &mut [0.0], &mut [maximum]).map_err(vips_error)?;
    let below = ops::relational_const(image, ops::OperationRelational::Less, &mut [minimum])
        .map_err(vips_error)?;
    let above = ops::relational_const(image, ops::OperationRelational::More, &mut [maximum])
        .map_err(vips_error)?;
    let clamped = ops::ifthenelse(&below, &minimum_image, image).map_err(vips_error)?;
    ops::ifthenelse(&above, &maximum_image, &clamped).map_err(vips_error)
}

fn smoothstep_image(image: &VipsImage) -> Result<VipsImage, RenderError> {
    let squared = ops::multiply(image, image).map_err(vips_error)?;
    let three_minus_twice = ops::linear(image, &mut [-2.0], &mut [3.0]).map_err(vips_error)?;
    ops::multiply(&squared, &three_minus_twice).map_err(vips_error)
}

fn split_color_and_alpha(
    image: &VipsImage,
    operation_name: &str,
) -> Result<(VipsImage, Option<VipsImage>), RenderError> {
    let bands = image.get_bands();
    let has_alpha = image.hasalpha();
    let color = if has_alpha {
        ops::extract_band_with_opts(image, 0, &ops::ExtractBandOptions { n: bands - 1 })
            .map_err(vips_error)?
    } else {
        ops::copy(image).map_err(vips_error)?
    };
    if color.get_bands() != 3 {
        return Err(RenderError::Engine(format!(
            "{operation_name} requires three color bands, got {}",
            color.get_bands()
        )));
    }
    let alpha = if has_alpha {
        Some(ops::extract_band(image, bands - 1).map_err(vips_error)?)
    } else {
        None
    };
    Ok((color, alpha))
}

fn join_alpha(color: VipsImage, alpha: Option<VipsImage>) -> Result<VipsImage, RenderError> {
    if let Some(alpha) = alpha {
        ops::bandjoin(&mut [color, alpha]).map_err(vips_error)
    } else {
        Ok(color)
    }
}

fn linear_rgb_to_oklab(color: &VipsImage) -> Result<VipsImage, RenderError> {
    let lms = recombine_three_channels(color, LINEAR_RGB_TO_LMS)?;
    let lms_absolute = ops::abs(&lms).map_err(vips_error)?;
    let lms_root = ops::math2_const(&lms_absolute, ops::OperationMath2::Pow, &mut [1.0 / 3.0])
        .map_err(vips_error)?;
    let negative = ops::relational_const(&lms, ops::OperationRelational::Less, &mut [0.0])
        .map_err(vips_error)?;
    let negative_root = ops::linear(&lms_root, &mut [-1.0], &mut [0.0]).map_err(vips_error)?;
    let signed_root = ops::ifthenelse(&negative, &negative_root, &lms_root).map_err(vips_error)?;

    recombine_three_channels(&signed_root, LMS_ROOT_TO_OKLAB)
}

fn oklab_to_linear_rgb(oklab: &VipsImage) -> Result<VipsImage, RenderError> {
    let lms_root = recombine_three_channels(oklab, OKLAB_TO_LMS_ROOT)?;
    let squared = ops::multiply(&lms_root, &lms_root).map_err(vips_error)?;
    let cubed = ops::multiply(&squared, &lms_root).map_err(vips_error)?;
    recombine_three_channels(&cubed, LMS_TO_LINEAR_RGB)
}

fn recombine_three_channels(
    image: &VipsImage,
    coefficients: [[f32; 3]; 3],
) -> Result<VipsImage, RenderError> {
    let mut bytes = Vec::with_capacity(9 * size_of::<f32>());
    for row in coefficients {
        for coefficient in row {
            bytes.extend_from_slice(&coefficient.to_ne_bytes());
        }
    }
    let matrix = VipsImage::new_from_memory_copy(&bytes, 3, 3, 1, ops::BandFormat::Float)
        .map_err(vips_error)?;
    ops::recomb(image, &matrix).map_err(vips_error)
}

fn white_balance_rgb_matrix(adjustment: WhiteBalance) -> Result<[[f32; 3]; 3], RenderError> {
    const D65_XY: (f64, f64) = (0.3127, 0.3290);
    const D65_KELVIN: f64 = 6504.0;
    const MIRED_RANGE: f64 = 120.0;
    const TINT_DUV_RANGE: f64 = 0.02;
    const CAT16: [[f64; 3]; 3] = [
        [0.401_288, 0.650_173, -0.051_461],
        [-0.250_268, 1.204_414, 0.045_854],
        [-0.002_079, 0.048_952, 0.953_127],
    ];
    const RGB_TO_XYZ: [[f64; 3]; 3] = [
        [0.412_456_4, 0.357_576_1, 0.180_437_5],
        [0.212_672_9, 0.715_152_2, 0.072_175_0],
        [0.019_333_9, 0.119_192_0, 0.950_304_1],
    ];
    const XYZ_TO_RGB: [[f64; 3]; 3] = [
        [3.240_454_2, -1.537_138_5, -0.498_531_4],
        [-0.969_266, 1.876_010_8, 0.041_556],
        [0.055_643_4, -0.204_025_9, 1.057_225_2],
    ];

    let base_mired = 1_000_000.0 / D65_KELVIN;
    let target_mired = base_mired + f64::from(adjustment.temperature) * MIRED_RANGE / 100.0;
    let target_kelvin = 1_000_000.0 / target_mired;
    let target_xy = temperature_tint_xy(
        D65_XY,
        D65_KELVIN,
        target_kelvin,
        -f64::from(adjustment.tint) * TINT_DUV_RANGE / 100.0,
    );
    let source_white = xy_to_xyz(D65_XY);
    let target_white = xy_to_xyz(target_xy);
    let source_cone = matrix_vector(CAT16, source_white);
    let target_cone = matrix_vector(CAT16, target_white);
    if source_cone
        .into_iter()
        .any(|value| value.abs() < f64::EPSILON)
    {
        return Err(RenderError::Engine(
            "CAT16 source white produced a zero cone response".into(),
        ));
    }
    let scale = [
        [target_cone[0] / source_cone[0], 0.0, 0.0],
        [0.0, target_cone[1] / source_cone[1], 0.0],
        [0.0, 0.0, target_cone[2] / source_cone[2]],
    ];
    let cat_inverse = inverse_3x3(CAT16)
        .ok_or_else(|| RenderError::Engine("CAT16 matrix could not be inverted".into()))?;
    let adaptation = matrix_multiply(matrix_multiply(cat_inverse, scale), CAT16);
    let rgb_matrix = matrix_multiply(matrix_multiply(XYZ_TO_RGB, adaptation), RGB_TO_XYZ);
    Ok(rgb_matrix.map(|row| row.map(|value| value as f32)))
}

fn temperature_tint_xy(
    base_white_xy: (f64, f64),
    base_kelvin: f64,
    kelvin: f64,
    duv: f64,
) -> (f64, f64) {
    let kelvin = kelvin.clamp(1_667.0, 25_000.0);
    let base_locus_uv = xy_to_uv(planckian_xy(base_kelvin));
    let target_locus_uv = xy_to_uv(planckian_xy(kelvin));
    let base_white_uv = xy_to_uv(base_white_xy);
    let shifted_uv = (
        base_white_uv.0 + target_locus_uv.0 - base_locus_uv.0,
        base_white_uv.1 + target_locus_uv.1 - base_locus_uv.1,
    );
    if duv.abs() < f64::EPSILON {
        return uv_to_xy(shifted_uv);
    }
    let mired = 1_000_000.0 / kelvin;
    let delta = 0.1;
    let before = planckian_xy(1_000_000.0 / (mired - delta));
    let after = planckian_xy(1_000_000.0 / (mired + delta));
    let before_uv = xy_to_uv(before);
    let after_uv = xy_to_uv(after);
    let tangent = (after_uv.0 - before_uv.0, after_uv.1 - before_uv.1);
    let length = tangent.0.hypot(tangent.1);
    let normal = (-tangent.1 / length, tangent.0 / length);
    uv_to_xy((shifted_uv.0 + duv * normal.0, shifted_uv.1 + duv * normal.1))
}

fn planckian_xy(kelvin: f64) -> (f64, f64) {
    let x = if kelvin <= 4_000.0 {
        -0.266_123_9e9 / kelvin.powi(3) - 0.234_358e6 / kelvin.powi(2)
            + 0.877_695_6e3 / kelvin
            + 0.179_910
    } else {
        -3.025_846_9e9 / kelvin.powi(3)
            + 2.107_037_9e6 / kelvin.powi(2)
            + 0.222_634_7e3 / kelvin
            + 0.240_390
    };
    let y = if kelvin <= 2_222.0 {
        -1.106_381_4 * x.powi(3) - 1.348_110_2 * x.powi(2) + 2.185_558_32 * x - 0.202_196_83
    } else if kelvin <= 4_000.0 {
        -0.954_947_6 * x.powi(3) - 1.374_185_93 * x.powi(2) + 2.091_370_15 * x - 0.167_488_67
    } else {
        3.081_758 * x.powi(3) - 5.873_386_7 * x.powi(2) + 3.751_129_97 * x - 0.370_014_83
    };
    (x, y)
}

fn xy_to_uv((x, y): (f64, f64)) -> (f64, f64) {
    let denominator = -2.0 * x + 12.0 * y + 3.0;
    (4.0 * x / denominator, 6.0 * y / denominator)
}

fn uv_to_xy((u, v): (f64, f64)) -> (f64, f64) {
    let denominator = 2.0 * u - 8.0 * v + 4.0;
    (3.0 * u / denominator, 2.0 * v / denominator)
}

fn xy_to_xyz((x, y): (f64, f64)) -> [f64; 3] {
    [x / y, 1.0, (1.0 - x - y) / y]
}

fn matrix_vector(matrix: [[f64; 3]; 3], vector: [f64; 3]) -> [f64; 3] {
    matrix.map(|row| row[0] * vector[0] + row[1] * vector[1] + row[2] * vector[2])
}

fn matrix_multiply(left: [[f64; 3]; 3], right: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    std::array::from_fn(|row| {
        std::array::from_fn(|column| {
            (0..3)
                .map(|index| left[row][index] * right[index][column])
                .sum()
        })
    })
}

fn inverse_3x3(matrix: [[f64; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    let determinant = matrix[0][0] * (matrix[1][1] * matrix[2][2] - matrix[1][2] * matrix[2][1])
        - matrix[0][1] * (matrix[1][0] * matrix[2][2] - matrix[1][2] * matrix[2][0])
        + matrix[0][2] * (matrix[1][0] * matrix[2][1] - matrix[1][1] * matrix[2][0]);
    if determinant.abs() < f64::EPSILON {
        return None;
    }
    let inverse_determinant = 1.0 / determinant;
    Some([
        [
            (matrix[1][1] * matrix[2][2] - matrix[1][2] * matrix[2][1]) * inverse_determinant,
            (matrix[0][2] * matrix[2][1] - matrix[0][1] * matrix[2][2]) * inverse_determinant,
            (matrix[0][1] * matrix[1][2] - matrix[0][2] * matrix[1][1]) * inverse_determinant,
        ],
        [
            (matrix[1][2] * matrix[2][0] - matrix[1][0] * matrix[2][2]) * inverse_determinant,
            (matrix[0][0] * matrix[2][2] - matrix[0][2] * matrix[2][0]) * inverse_determinant,
            (matrix[0][2] * matrix[1][0] - matrix[0][0] * matrix[1][2]) * inverse_determinant,
        ],
        [
            (matrix[1][0] * matrix[2][1] - matrix[1][1] * matrix[2][0]) * inverse_determinant,
            (matrix[0][1] * matrix[2][0] - matrix[0][0] * matrix[2][1]) * inverse_determinant,
            (matrix[0][0] * matrix[1][1] - matrix[0][1] * matrix[1][0]) * inverse_determinant,
        ],
    ])
}

fn crop_image(image: &VipsImage, crop: CropRect) -> Result<VipsImage, RenderError> {
    let info = dimensions(image)?;
    let image_width = info.width as f32;
    let image_height = info.height as f32;
    let left = (crop.x * image_width).floor() as i32;
    let top = (crop.y * image_height).floor() as i32;
    let right = ((crop.x + crop.width) * image_width).ceil() as i32;
    let bottom = ((crop.y + crop.height) * image_height).ceil() as i32;
    let width = (right.clamp(left + 1, info.width as i32) - left).max(1);
    let height = (bottom.clamp(top + 1, info.height as i32) - top).max(1);
    ops::extract_area(image, left, top, width, height).map_err(vips_error)
}

fn apply_exposure_linear(image: &VipsImage, ev: f32) -> Result<VipsImage, RenderError> {
    let factor = 2_f64.powf(f64::from(ev));
    let bands = image.get_bands();
    let has_alpha = image.hasalpha();
    if has_alpha {
        let color =
            ops::extract_band_with_opts(image, 0, &ops::ExtractBandOptions { n: bands - 1 })
                .map_err(vips_error)?;
        let alpha = ops::extract_band(image, bands - 1).map_err(vips_error)?;
        let exposed = ops::linear(&color, &mut [factor], &mut [0.0]).map_err(vips_error)?;
        ops::bandjoin(&mut [exposed, alpha]).map_err(vips_error)
    } else {
        ops::linear(image, &mut [factor], &mut [0.0]).map_err(vips_error)
    }
}

fn apply_tone_curve_linear(image: &VipsImage, curve: ToneCurve) -> Result<VipsImage, RenderError> {
    let bands = image.get_bands();
    let has_alpha = image.hasalpha();
    let color = if has_alpha {
        ops::extract_band_with_opts(image, 0, &ops::ExtractBandOptions { n: bands - 1 })
            .map_err(vips_error)?
    } else {
        ops::copy(image).map_err(vips_error)?
    };
    let alpha = if has_alpha {
        Some(ops::extract_band(image, bands - 1).map_err(vips_error)?)
    } else {
        None
    };

    // Casting to ushort saturates the lookup index to 0..=65535 on libvips
    // 8.15, avoiding the newer vips_clamp symbol unavailable on Ubuntu 24.04.
    let index = ops::linear(&color, &mut [65_535.0], &mut [0.0]).map_err(vips_error)?;
    let index = ops::cast(&index, ops::BandFormat::Ushort).map_err(vips_error)?;
    let lut = tone_curve_lut(curve)?;
    let mapped = ops::maplut(&index, &lut).map_err(vips_error)?;

    // Preserve extended-range scRGB values. The editable curve is explicitly
    // display-referred over 0..=1 and must not destroy negative or HDR data.
    let below = ops::relational_const(&color, ops::OperationRelational::Less, &mut [0.0])
        .map_err(vips_error)?;
    let above = ops::relational_const(&color, ops::OperationRelational::More, &mut [1.0])
        .map_err(vips_error)?;
    let mapped = ops::ifthenelse(&below, &color, &mapped).map_err(vips_error)?;
    let mapped = ops::ifthenelse(&above, &color, &mapped).map_err(vips_error)?;

    if let Some(alpha) = alpha {
        ops::bandjoin(&mut [mapped, alpha]).map_err(vips_error)
    } else {
        Ok(mapped)
    }
}

fn tone_curve_lut(curve: ToneCurve) -> Result<VipsImage, RenderError> {
    let mut bytes = Vec::with_capacity(65_536 * size_of::<f32>());
    for index in 0..=u16::MAX {
        let sample = curve.sample(f32::from(index) / 65_535.0);
        bytes.extend_from_slice(&sample.to_ne_bytes());
    }
    VipsImage::new_from_memory_copy(&bytes, 65_536, 1, 1, ops::BandFormat::Float)
        .map_err(vips_error)
}

fn render_viewport(image: &VipsImage, viewport: Viewport) -> Result<(VipsImage, f32), RenderError> {
    let info = dimensions(image)?;
    let image_width = f64::from(info.width);
    let image_height = f64::from(info.height);
    let output_width = f64::from(viewport.output_width);
    let output_height = f64::from(viewport.output_height);
    let fit_scale = (output_width / image_width)
        .min(output_height / image_height)
        .min(1.0);
    let scale = if viewport.zoom <= 0.0 {
        fit_scale
    } else {
        f64::from(viewport.zoom.clamp(0.01, 32.0))
    };

    if viewport.zoom <= 0.0 {
        let resized = if (scale - 1.0).abs() < f64::EPSILON {
            ops::copy(image).map_err(vips_error)?
        } else {
            ops::resize(image, scale).map_err(vips_error)?
        };
        return Ok((resized, scale as f32));
    }

    let crop_width = (output_width / scale).ceil().min(image_width).max(1.0) as i32;
    let crop_height = (output_height / scale).ceil().min(image_height).max(1.0) as i32;
    let center_x = f64::from(viewport.center_x.clamp(0.0, 1.0)) * image_width;
    let center_y = f64::from(viewport.center_y.clamp(0.0, 1.0)) * image_height;
    let max_left = (info.width as i32 - crop_width).max(0);
    let max_top = (info.height as i32 - crop_height).max(0);
    let left = ((center_x - f64::from(crop_width) / 2.0).round() as i32).clamp(0, max_left);
    let top = ((center_y - f64::from(crop_height) / 2.0).round() as i32).clamp(0, max_top);
    let cropped =
        ops::extract_area(image, left, top, crop_width, crop_height).map_err(vips_error)?;
    let resized = if (scale - 1.0).abs() < f64::EPSILON {
        cropped
    } else {
        ops::resize(&cropped, scale).map_err(vips_error)?
    };
    Ok((resized, scale as f32))
}

fn ensure_rgba8(image: &VipsImage) -> Result<VipsImage, RenderError> {
    let cast = ops::cast(image, ops::BandFormat::Uchar).map_err(vips_error)?;
    match cast.get_bands() {
        4 => Ok(cast),
        3 => ops::addalpha(&cast).map_err(vips_error),
        bands => Err(RenderError::Engine(format!(
            "expected an RGB or RGBA image after colour conversion, got {bands} bands"
        ))),
    }
}

fn fit_to_canvas(image: &VipsImage, viewport: Viewport) -> Result<VipsImage, RenderError> {
    let info = dimensions(image)?;
    let canvas_width =
        i32::try_from(viewport.output_width).map_err(|_| RenderError::InvalidDimensions)?;
    let canvas_height =
        i32::try_from(viewport.output_height).map_err(|_| RenderError::InvalidDimensions)?;
    let visible_width = info.width.min(viewport.output_width);
    let visible_height = info.height.min(viewport.output_height);
    let visible = if visible_width != info.width || visible_height != info.height {
        let left = ((info.width - visible_width) / 2) as i32;
        let top = ((info.height - visible_height) / 2) as i32;
        ops::extract_area(
            image,
            left,
            top,
            visible_width as i32,
            visible_height as i32,
        )
        .map_err(vips_error)?
    } else {
        ops::copy(image).map_err(vips_error)?
    };
    let left = (canvas_width - visible_width as i32) / 2;
    let top = (canvas_height - visible_height as i32) / 2;
    ops::embed_with_opts(
        &visible,
        left,
        top,
        canvas_width,
        canvas_height,
        &ops::EmbedOptions {
            extend: ops::Extend::Background,
            background: vec![0.0, 0.0, 0.0, 0.0],
        },
    )
    .map_err(vips_error)
}

fn dimensions(image: &VipsImage) -> Result<ImageInfo, RenderError> {
    let width = u32::try_from(image.get_width()).map_err(|_| RenderError::InvalidDimensions)?;
    let height = u32::try_from(image.get_height()).map_err(|_| RenderError::InvalidDimensions)?;
    if width == 0 || height == 0 {
        return Err(RenderError::InvalidDimensions);
    }
    Ok(ImageInfo { width, height })
}

fn validate_viewport(viewport: Viewport) -> Result<(), RenderError> {
    if viewport.output_width == 0
        || viewport.output_height == 0
        || !viewport.zoom.is_finite()
        || !viewport.center_x.is_finite()
        || !viewport.center_y.is_finite()
    {
        Err(RenderError::InvalidDimensions)
    } else {
        Ok(())
    }
}

fn path_string(path: &Path) -> Result<String, RenderError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| RenderError::Engine(format!("path is not valid UTF-8: {}", path.display())))
}

fn export_temporary_path(destination: &Path) -> PathBuf {
    let stem = destination
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let extension = destination
        .extension()
        .map(|value| value.to_string_lossy())
        .unwrap_or_default();
    let name = if extension.is_empty() {
        format!("{stem}.focusless-export.tmp")
    } else {
        format!("{stem}.focusless-export.tmp.{extension}")
    };
    destination.with_file_name(name)
}

#[cfg(not(target_os = "windows"))]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(target_os = "windows")]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn vips_error(error: VipsError) -> RenderError {
    RenderError::Engine(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_exposure_and_exports_all_supported_formats() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.png");
        let mut engine = VipsEngine::new().unwrap();

        let pixels = [
            64_u8, 32, 16, 255, 128, 64, 32, 128, 255, 128, 64, 255, 32, 16, 8, 255,
        ];
        let image =
            VipsImage::new_from_memory_copy(&pixels, 2, 2, 4, ops::BandFormat::Uchar).unwrap();
        ops::pngsave(&image, source_path.to_str().unwrap()).unwrap();

        let result = engine
            .render_preview(&PreviewRequest {
                generation: 7,
                source_path: source_path.clone(),
                operations: vec![Operation::Exposure { ev: 1.0 }],
                viewport: Viewport::fit(2, 2),
            })
            .unwrap();
        assert_eq!(result.generation, 7);
        assert_eq!((result.width, result.height), (2, 2));
        assert!(
            (i16::from(result.rgba8[0]) - i16::from(expected_srgb_exposure(64, 1.0))).abs() <= 1
        );
        assert!(
            (i16::from(result.rgba8[4]) - i16::from(expected_srgb_exposure(128, 1.0))).abs() <= 1
        );
        assert_eq!(result.rgba8[3], 255);
        assert_eq!(result.rgba8[7], 128, "exposure must preserve alpha");
        assert_eq!(engine.fit_source_builds, 1);

        let copied = engine
            .copy_clipboard(
                &CopyRequest {
                    source_path: source_path.clone(),
                    operations: vec![Operation::Exposure { ev: 1.0 }],
                },
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!((copied.width, copied.height), (2, 2));
        assert_eq!(copied.rgba8.len(), 2 * 2 * 4);
        assert!(
            (i16::from(copied.rgba8[0]) - i16::from(expected_srgb_exposure(64, 1.0))).abs() <= 1,
            "clipboard copy must contain the full-resolution edited image"
        );
        assert_eq!(copied.rgba8[7], 128, "clipboard copy must preserve alpha");

        let framed = engine
            .render_preview(&PreviewRequest {
                generation: 8,
                source_path: source_path.clone(),
                operations: vec![
                    Operation::Exposure { ev: 1.0 },
                    Operation::Frame {
                        width_pct: 10.0,
                        color: FrameColor::BLACK,
                    },
                ],
                viewport: Viewport::fit(2, 2),
            })
            .unwrap();
        assert_eq!((framed.width, framed.height), (2, 2));
        assert_eq!(
            engine.fit_source_builds, 1,
            "all fitted previews must reuse the materialized source proxy"
        );

        let darker = engine
            .render_preview(&PreviewRequest {
                generation: 9,
                source_path: source_path.clone(),
                operations: vec![Operation::Exposure { ev: -1.0 }],
                viewport: Viewport::fit(2, 2),
            })
            .unwrap();
        assert!(
            (i16::from(darker.rgba8[0]) - i16::from(expected_srgb_exposure(64, -1.0))).abs() <= 1
        );
        assert_eq!(darker.rgba8[7], 128, "negative EV must preserve alpha");
        assert_eq!(
            engine.fit_source_builds, 1,
            "all fit-preview adjustments must reuse the source proxy"
        );

        let contrasted = engine
            .render_preview(&PreviewRequest {
                generation: 10,
                source_path: source_path.clone(),
                operations: vec![Operation::Contrast { amount: 100.0 }],
                viewport: Viewport::fit(2, 2),
            })
            .unwrap();
        assert!(
            contrasted.rgba8[0] < pixels[0],
            "positive contrast must darken a dark sample"
        );
        assert!(
            contrasted.rgba8[8] >= pixels[8],
            "positive contrast must not darken the brightest sample"
        );
        assert_eq!(
            contrasted.rgba8[7], 128,
            "contrast must preserve the alpha channel"
        );

        let mut extended_bytes = Vec::new();
        for value in [-0.1_f32, -0.1, -0.1, 1.1, 1.1, 1.1] {
            extended_bytes.extend_from_slice(&value.to_ne_bytes());
        }
        let extended =
            VipsImage::new_from_memory_copy(&extended_bytes, 2, 1, 3, ops::BandFormat::Float)
                .unwrap();
        let preserved = apply_contrast_oklab(&extended, 100.0).unwrap();
        let preserved = ops::cast(&preserved, ops::BandFormat::Float).unwrap();
        let preserved_bytes = preserved.write_to_memory();
        let preserved_values = preserved_bytes
            .chunks_exact(size_of::<f32>())
            .map(|bytes| f32::from_ne_bytes(bytes.try_into().unwrap()))
            .collect::<Vec<_>>();
        for (actual, expected) in preserved_values
            .iter()
            .zip([-0.1_f32, -0.1, -0.1, 1.1, 1.1, 1.1])
        {
            assert!(
                (*actual - expected).abs() < 2.0e-5,
                "contrast changed extended-range value {expected} to {actual}"
            );
        }

        let curve = ToneCurve {
            shadow_input: 0.2,
            shadows: 0.12,
            midtone_input: 0.55,
            midtones: 0.42,
            highlight_input: 0.82,
            highlights: 0.88,
        };
        let curved = engine
            .render_preview(&PreviewRequest {
                generation: 9,
                source_path: source_path.clone(),
                operations: vec![Operation::ToneCurve { curve }],
                viewport: Viewport::fit(2, 2),
            })
            .unwrap();
        assert!(
            (i16::from(curved.rgba8[0]) - i16::from(expected_srgb_curve(64, curve))).abs() <= 1
        );
        assert_eq!(curved.rgba8[7], 128, "tone curve must preserve alpha");

        let balanced = engine
            .render_preview(&PreviewRequest {
                generation: 10,
                source_path: source_path.clone(),
                operations: vec![Operation::WhiteBalance {
                    adjustment: WhiteBalance {
                        temperature: 65.0,
                        tint: 0.0,
                    },
                }],
                viewport: Viewport::fit(2, 2),
            })
            .unwrap();
        assert_eq!(balanced.rgba8[7], 128, "white balance must preserve alpha");
        assert!(
            balanced.rgba8[0] > pixels[0],
            "warming should raise the red component for this source pixel"
        );
        assert!(
            balanced.rgba8[2] < pixels[2],
            "warming should lower the blue component for this source pixel"
        );

        let desaturated = engine
            .render_preview(&PreviewRequest {
                generation: 11,
                source_path: source_path.clone(),
                operations: vec![Operation::Saturation { amount: -100.0 }],
                viewport: Viewport::fit(2, 2),
            })
            .unwrap();
        assert!(
            channel_range(&desaturated.rgba8[0..3]) <= 1,
            "-100 saturation must produce a neutral pixel"
        );
        assert_eq!(desaturated.rgba8[7], 128, "saturation must preserve alpha");

        let saturated = engine
            .render_preview(&PreviewRequest {
                generation: 12,
                source_path: source_path.clone(),
                operations: vec![Operation::Saturation { amount: 100.0 }],
                viewport: Viewport::fit(2, 2),
            })
            .unwrap();
        assert!(
            channel_range(&saturated.rgba8[0..3]) > channel_range(&pixels[0..3]),
            "+100 saturation must increase chroma for a colored pixel"
        );

        for (name, format) in [
            ("out.jpg", ExportFormat::Jpeg { quality: 92 }),
            ("out.png", ExportFormat::Png),
            ("out.webp", ExportFormat::WebP { quality: 90 }),
        ] {
            let destination = directory.path().join(name);
            engine
                .export(
                    &ExportRequest {
                        source_path: source_path.clone(),
                        destination_path: destination.clone(),
                        operations: vec![
                            Operation::Rotate { quarter_turns: 1 },
                            Operation::Crop {
                                rect: CropRect {
                                    x: 0.0,
                                    y: 0.0,
                                    width: 0.5,
                                    height: 1.0,
                                },
                            },
                            Operation::WhiteBalance {
                                adjustment: WhiteBalance {
                                    temperature: 18.0,
                                    tint: -7.0,
                                },
                            },
                            Operation::Exposure { ev: 0.5 },
                            Operation::Saturation { amount: 24.0 },
                            Operation::Sharpness { amount: 145.0 },
                        ],
                        format,
                    },
                    &AtomicBool::new(false),
                )
                .unwrap();
            assert!(destination.metadata().unwrap().len() > 0);
            let exported = VipsImage::new_from_file(destination.to_str().unwrap()).unwrap();
            assert!(
                exported.get_blob("icc-profile-data").is_ok(),
                "exports must include the sRGB ICC profile"
            );
            assert_eq!(
                engine.inspect(&destination).unwrap(),
                ImageInfo {
                    width: 1,
                    height: 2
                }
            );
        }

        let replacement = directory.path().join("out.png");
        let previous_bytes = fs::read(&replacement).unwrap();
        engine
            .export(
                &ExportRequest {
                    source_path: source_path.clone(),
                    destination_path: replacement.clone(),
                    operations: vec![Operation::Exposure { ev: -0.5 }],
                    format: ExportFormat::Png,
                },
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_ne!(
            fs::read(&replacement).unwrap(),
            previous_bytes,
            "exporting over an existing destination must replace its bytes"
        );

        let source_path = directory.path().join("geometry.png");
        let pixels = vec![128_u8; 4 * 2 * 4];
        let image =
            VipsImage::new_from_memory_copy(&pixels, 4, 2, 4, ops::BandFormat::Uchar).unwrap();
        ops::pngsave(&image, source_path.to_str().unwrap()).unwrap();

        let result = engine
            .render_preview(&PreviewRequest {
                generation: 9,
                source_path,
                operations: vec![
                    Operation::Rotate { quarter_turns: 1 },
                    Operation::Crop {
                        rect: CropRect {
                            x: 0.0,
                            y: 0.0,
                            width: 0.5,
                            height: 1.0,
                        },
                    },
                ],
                viewport: Viewport::fit(1, 4),
            })
            .unwrap();

        assert_eq!((result.width, result.height), (1, 4));

        let source_path = directory.path().join("sharpness.png");
        let mut pixels = Vec::with_capacity(9 * 3 * 4);
        for y in 0..3 {
            for x in 0..9 {
                let value = if x < 4 { 64 } else { 192 };
                let alpha = if x == 4 && y == 1 { 128 } else { 255 };
                pixels.extend_from_slice(&[value, value, value, alpha]);
            }
        }
        let image =
            VipsImage::new_from_memory_copy(&pixels, 9, 3, 4, ops::BandFormat::Uchar).unwrap();
        ops::pngsave(&image, source_path.to_str().unwrap()).unwrap();

        let baseline = engine
            .render_preview(&PreviewRequest {
                generation: 20,
                source_path: source_path.clone(),
                operations: vec![],
                viewport: Viewport::fit(9, 3),
            })
            .unwrap();
        let sharpened = engine
            .render_preview(&PreviewRequest {
                generation: 21,
                source_path,
                operations: vec![Operation::Sharpness { amount: 1000.0 }],
                viewport: Viewport::fit(9, 3),
            })
            .unwrap();

        let dark_edge = (9 + 3) * 4;
        let bright_edge = (9 + 4) * 4;
        assert!(sharpened.rgba8[dark_edge] < baseline.rgba8[dark_edge]);
        assert!(sharpened.rgba8[bright_edge] > baseline.rgba8[bright_edge]);
        assert_eq!(
            &sharpened.rgba8[dark_edge..dark_edge + 3],
            &[
                sharpened.rgba8[dark_edge],
                sharpened.rgba8[dark_edge],
                sharpened.rgba8[dark_edge],
            ],
            "luminance-only sharpening must not color a neutral edge"
        );
        assert_eq!(
            sharpened.rgba8[bright_edge + 3],
            128,
            "sharpness must preserve alpha"
        );

        let mut straightening_bytes = Vec::with_capacity(20 * 10 * 4 * size_of::<f32>());
        for _ in 0..20 * 10 {
            for value in [0.2_f32, 0.4, 0.6, 0.5] {
                straightening_bytes.extend_from_slice(&value.to_ne_bytes());
            }
        }
        let straightening_source = VipsImage::new_from_memory_copy(
            &straightening_bytes,
            20,
            10,
            4,
            ops::BandFormat::Float,
        )
        .unwrap();
        let straightened = apply_straighten_linear(&straightening_source, 12.0).unwrap();
        let straightened_info = dimensions(&straightened).unwrap();
        assert!(straightened_info.width < 20 && straightened_info.height < 10);
        let alpha = ops::extract_band(&straightened, 3).unwrap();
        let alpha = ops::cast(&alpha, ops::BandFormat::Float).unwrap();
        for bytes in alpha.write_to_memory().chunks_exact(size_of::<f32>()) {
            let value = f32::from_ne_bytes(bytes.try_into().unwrap());
            assert!(
                (value - 0.5).abs() < 0.01,
                "straightening changed source alpha to {value}"
            );
        }
    }

    #[test]
    fn exports_grayscale_alpha_as_jpeg() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("grayscale-alpha.png");
        let destination = directory.path().join("grayscale-alpha.jpg");
        let pixels = [32_u8, 255, 128, 128, 224, 64, 255, 0];
        let image =
            VipsImage::new_from_memory_copy(&pixels, 2, 2, 2, ops::BandFormat::Uchar).unwrap();
        ops::pngsave(&image, source_path.to_str().unwrap()).unwrap();

        let engine = VipsEngine::new().unwrap();
        engine
            .export(
                &ExportRequest {
                    source_path,
                    destination_path: destination.clone(),
                    operations: vec![],
                    format: ExportFormat::Jpeg { quality: 92 },
                },
                &AtomicBool::new(false),
            )
            .unwrap();

        let exported = VipsImage::new_from_file(destination.to_str().unwrap()).unwrap();
        assert_eq!(exported.get_bands(), 3);
        assert!(exported.get_blob("icc-profile-data").is_ok());
    }

    #[test]
    fn exports_opaque_rgb_as_jpeg_without_flattening() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("opaque-rgb.png");
        let destination = directory.path().join("opaque-rgb.jpg");
        let pixels = [32_u8, 64, 96, 128, 160, 192, 224, 240, 255, 12, 24, 48];
        let image =
            VipsImage::new_from_memory_copy(&pixels, 2, 2, 3, ops::BandFormat::Uchar).unwrap();
        ops::pngsave(&image, source_path.to_str().unwrap()).unwrap();

        let engine = VipsEngine::new().unwrap();
        engine
            .export(
                &ExportRequest {
                    source_path,
                    destination_path: destination.clone(),
                    operations: vec![Operation::WhiteBalance {
                        adjustment: WhiteBalance {
                            temperature: 12.0,
                            tint: -20.0,
                        },
                    }],
                    format: ExportFormat::Jpeg { quality: 92 },
                },
                &AtomicBool::new(false),
            )
            .unwrap();

        let exported = VipsImage::new_from_file(destination.to_str().unwrap()).unwrap();
        assert_eq!(exported.get_bands(), 3);
        assert!(exported.get_blob("icc-profile-data").is_ok());
    }

    #[test]
    fn cat16_white_balance_has_expected_neutral_axis_behavior() {
        let identity = white_balance_rgb_matrix(WhiteBalance::IDENTITY).unwrap();
        for (row, values) in identity.iter().enumerate() {
            for (column, value) in values.iter().enumerate() {
                let expected = if row == column { 1.0 } else { 0.0 };
                assert!(
                    (*value - expected).abs() < 2.0e-6,
                    "identity coefficient ({row}, {column}) was {value}"
                );
            }
        }

        let neutral = [0.18_f32; 3];
        let warm = apply_test_matrix(
            white_balance_rgb_matrix(WhiteBalance {
                temperature: 100.0,
                tint: 0.0,
            })
            .unwrap(),
            neutral,
        );
        let cool = apply_test_matrix(
            white_balance_rgb_matrix(WhiteBalance {
                temperature: -100.0,
                tint: 0.0,
            })
            .unwrap(),
            neutral,
        );
        assert!(warm[0] > warm[1] && warm[1] > warm[2], "{warm:?}");
        assert!(cool[2] > cool[1] && cool[1] > cool[0], "{cool:?}");

        let magenta = apply_test_matrix(
            white_balance_rgb_matrix(WhiteBalance {
                temperature: 0.0,
                tint: 100.0,
            })
            .unwrap(),
            neutral,
        );
        assert!(magenta[1] < (magenta[0] + magenta[2]) * 0.5, "{magenta:?}");

        for adjusted in [warm, cool, magenta] {
            let luminance =
                0.212_672_9 * adjusted[0] + 0.715_152_2 * adjusted[1] + 0.072_175 * adjusted[2];
            assert!(
                (luminance - 0.18).abs() < 2.0e-5,
                "CAT16 must preserve the reference-white luminance, got {luminance}"
            );
        }
    }

    #[test]
    fn guided_shadows_and_highlights_behaves_correctly() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("llf-test.png");

        // Create a test image with distinct dark, mid, and bright areas
        // and an alpha channel.
        // Left column (dark): L ~ 0.1
        // Middle column (mid): L ~ 0.5
        // Right column (bright): L ~ 0.9
        let dark = encode_srgb(0.01); // L ~ 0.1
        let mid = encode_srgb(0.18); // L ~ 0.5
        let bright = encode_srgb(0.8); // L ~ 0.9
        let pixels = vec![
            dark, dark, dark, 200, mid, mid, mid, 255, bright, bright, bright, 100, dark, dark,
            dark, 200, mid, mid, mid, 255, bright, bright, bright, 100,
        ];
        let image =
            VipsImage::new_from_memory_copy(&pixels, 3, 2, 4, ops::BandFormat::Uchar).unwrap();
        ops::pngsave(&image, source_path.to_str().unwrap()).unwrap();

        let mut engine = VipsEngine::new().unwrap();

        // 1. Identity
        let baseline = engine
            .render_preview(&PreviewRequest {
                generation: 1,
                source_path: source_path.clone(),
                operations: vec![Operation::ShadowsHighlights {
                    adjustment: ShadowsHighlights::IDENTITY,
                }],
                viewport: Viewport::fit(3, 2),
            })
            .unwrap();
        for (i, (&actual, &expected)) in baseline.rgba8.iter().zip(&pixels).enumerate() {
            // Color-space round trips may introduce a one-code-value difference.
            assert!(
                (i16::from(actual) - i16::from(expected)).abs() <= 1,
                "identity adjustment changed pixel {i}: expected {expected}, got {actual}"
            );
        }

        // 2. Positive Shadows darkens dark areas.
        let deepened_shadows = engine
            .render_preview(&PreviewRequest {
                generation: 2,
                source_path: source_path.clone(),
                operations: vec![Operation::ShadowsHighlights {
                    adjustment: ShadowsHighlights {
                        shadows: 50.0,
                        highlights: 0.0,
                    },
                }],
                viewport: Viewport::fit(3, 2),
            })
            .unwrap();

        let dark_idx = 0; // First pixel is dark
        let bright_idx = 8; // Third pixel is bright

        assert!(
            deepened_shadows.rgba8[dark_idx] <= baseline.rgba8[dark_idx] - 5,
            "positive shadows must darken the dark sample. baseline: {}, deepened: {}",
            baseline.rgba8[dark_idx],
            deepened_shadows.rgba8[dark_idx]
        );
        assert_eq!(
            deepened_shadows.rgba8[dark_idx + 3],
            200,
            "shadows adjustment must preserve alpha"
        );
        assert!(
            (i16::from(deepened_shadows.rgba8[bright_idx]) - i16::from(baseline.rgba8[bright_idx]))
                .abs()
                <= 2,
            "positive shadows should barely affect bright samples"
        );

        let lifted_shadows = engine
            .render_preview(&PreviewRequest {
                generation: 3,
                source_path: source_path.clone(),
                operations: vec![Operation::ShadowsHighlights {
                    adjustment: ShadowsHighlights {
                        shadows: -50.0,
                        highlights: 0.0,
                    },
                }],
                viewport: Viewport::fit(3, 2),
            })
            .unwrap();
        assert!(
            lifted_shadows.rgba8[dark_idx] > baseline.rgba8[dark_idx] + 5,
            "negative shadows must lift the dark sample"
        );

        // 3. Positive Highlights brightens bright areas, matching the
        // conventional direction used by photo editors.
        let lifted_highlights = engine
            .render_preview(&PreviewRequest {
                generation: 4,
                source_path: source_path.clone(),
                operations: vec![Operation::ShadowsHighlights {
                    adjustment: ShadowsHighlights {
                        shadows: 0.0,
                        highlights: 50.0,
                    },
                }],
                viewport: Viewport::fit(3, 2),
            })
            .unwrap();

        assert!(
            lifted_highlights.rgba8[bright_idx] > baseline.rgba8[bright_idx] + 10,
            "positive highlights must brighten the bright sample. baseline: {}, lifted: {}",
            baseline.rgba8[bright_idx],
            lifted_highlights.rgba8[bright_idx]
        );
        assert_eq!(
            lifted_highlights.rgba8[bright_idx + 3],
            100,
            "highlights adjustment must preserve alpha"
        );
        assert!(
            (i16::from(lifted_highlights.rgba8[dark_idx]) - i16::from(baseline.rgba8[dark_idx]))
                .abs()
                <= 2,
            "highlights slider should barely affect dark samples"
        );

        let recovered_highlights = engine
            .render_preview(&PreviewRequest {
                generation: 5,
                source_path: source_path.clone(),
                operations: vec![Operation::ShadowsHighlights {
                    adjustment: ShadowsHighlights {
                        shadows: 0.0,
                        highlights: -50.0,
                    },
                }],
                viewport: Viewport::fit(3, 2),
            })
            .unwrap();
        assert!(
            recovered_highlights.rgba8[bright_idx] < baseline.rgba8[bright_idx] - 10,
            "negative highlights must recover the bright sample"
        );

        let mut extended_bytes = Vec::new();
        for value in [-0.1_f32, -0.1, -0.1, 1.1, 1.1, 1.1] {
            extended_bytes.extend_from_slice(&value.to_ne_bytes());
        }
        let extended =
            VipsImage::new_from_memory_copy(&extended_bytes, 2, 1, 3, ops::BandFormat::Float)
                .unwrap();
        let preserved = apply_shadows_highlights_guided(
            &extended,
            ShadowsHighlights {
                shadows: 100.0,
                highlights: 100.0,
            },
        )
        .unwrap();
        let preserved = ops::cast(&preserved, ops::BandFormat::Float).unwrap();
        let preserved_bytes = preserved.write_to_memory();
        let preserved_values = preserved_bytes
            .chunks_exact(size_of::<f32>())
            .map(|bytes| f32::from_ne_bytes(bytes.try_into().unwrap()))
            .collect::<Vec<_>>();
        for (actual, expected) in preserved_values
            .iter()
            .zip([-0.1_f32, -0.1, -0.1, 1.1, 1.1, 1.1])
        {
            assert!(
                actual.is_finite() && (*actual - expected).abs() < 2.0e-5,
                "shadows/highlights changed extended-range value {expected} to {actual}"
            );
        }
    }

    #[test]
    fn restored_fit_zoom_uses_the_source_proxy_on_the_first_preview() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("restored-fit.png");
        let pixels = vec![128_u8; 120 * 80 * 4];
        let image =
            VipsImage::new_from_memory_copy(&pixels, 120, 80, 4, ops::BandFormat::Uchar).unwrap();
        ops::pngsave(&image, source_path.to_str().unwrap()).unwrap();

        let mut engine = VipsEngine::new().unwrap();
        let result = engine
            .render_preview(&PreviewRequest {
                generation: 1,
                source_path,
                operations: vec![Operation::Exposure { ev: 0.5 }],
                viewport: Viewport {
                    output_width: 70,
                    output_height: 50,
                    zoom: 0.58,
                    center_x: 0.5,
                    center_y: 0.5,
                },
            })
            .unwrap();

        assert_eq!((result.width, result.height), (70, 50));
        assert_eq!(engine.fit_source_builds, 1);
        assert!(
            engine.preview_pipeline.is_none(),
            "a restored fit zoom must not build the full-resolution preview pipeline"
        );
    }

    #[test]
    fn downstream_edits_reuse_the_shadows_highlights_fit_stage() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("fit-stage-cache.png");
        let mut pixels = Vec::with_capacity(240 * 160 * 4);
        for y in 0..160_u32 {
            for x in 0..240_u32 {
                pixels.extend_from_slice(&[
                    (32 + x * 191 / 239) as u8,
                    (24 + y * 207 / 159) as u8,
                    (48 + (x + y) * 175 / 398) as u8,
                    255,
                ]);
            }
        }
        let image =
            VipsImage::new_from_memory_copy(&pixels, 240, 160, 4, ops::BandFormat::Uchar).unwrap();
        ops::pngsave(&image, source_path.to_str().unwrap()).unwrap();

        let shadows_highlights = Operation::ShadowsHighlights {
            adjustment: ShadowsHighlights {
                shadows: 45.0,
                highlights: -35.0,
            },
        };
        let mut engine = VipsEngine::new().unwrap();
        engine
            .render_preview(&PreviewRequest {
                generation: 1,
                source_path: source_path.clone(),
                operations: vec![shadows_highlights, Operation::Saturation { amount: 20.0 }],
                viewport: Viewport::fit(120, 80),
            })
            .unwrap();
        let operations = vec![shadows_highlights, Operation::Saturation { amount: 60.0 }];
        let request = PreviewRequest {
            generation: 2,
            source_path: source_path.clone(),
            operations: operations.clone(),
            viewport: Viewport::fit(120, 80),
        };
        let cached = engine.render_preview(&request).unwrap();
        assert_eq!(
            engine.fit_adjustment_builds, 1,
            "a downstream edit must reuse the materialized shadows/highlights stage"
        );

        let source = engine.fit_source.as_ref().unwrap();
        let adjusted =
            apply_operations_scaled(&source.image, &operations, source.source_scale).unwrap();
        let (visible, proxy_zoom) = render_viewport(&adjusted, request.viewport).unwrap();
        let reference =
            finalize_preview(&request, visible, source.source_scale * proxy_zoom).unwrap();
        assert_eq!(
            cached.rgba8, reference.rgba8,
            "the materialized stage must not change preview pixels"
        );

        engine
            .render_preview(&PreviewRequest {
                generation: 3,
                source_path,
                operations: vec![
                    Operation::Exposure { ev: 0.5 },
                    shadows_highlights,
                    Operation::Saturation { amount: 60.0 },
                ],
                viewport: Viewport::fit(120, 80),
            })
            .unwrap();
        assert_eq!(
            engine.fit_adjustment_builds, 2,
            "an upstream edit must rebuild the shadows/highlights stage"
        );
    }

    #[test]
    fn fit_source_proxy_matches_the_full_resolution_reference() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("frame-reference.png");
        let mut pixels = Vec::with_capacity(1200 * 800 * 4);
        for y in 0..800_u32 {
            for x in 0..1200_u32 {
                pixels.extend_from_slice(&[
                    (24 + x * 231 / 1199) as u8,
                    (32 + y * 223 / 799) as u8,
                    (48 + (x + y) * 207 / 1998) as u8,
                    (80 + x * 175 / 1199) as u8,
                ]);
            }
        }
        let image =
            VipsImage::new_from_memory_copy(&pixels, 1200, 800, 4, ops::BandFormat::Uchar).unwrap();
        ops::pngsave(&image, source_path.to_str().unwrap()).unwrap();

        let operations = vec![
            Operation::Exposure { ev: 0.75 },
            Operation::Frame {
                width_pct: 20.0,
                color: FrameColor {
                    r: 46,
                    g: 46,
                    b: 46,
                },
            },
        ];
        let viewport = Viewport::fit(700, 500);
        let mut engine = VipsEngine::new().unwrap();
        let cached = engine
            .render_preview(&PreviewRequest {
                generation: 1,
                source_path: source_path.clone(),
                operations: operations.clone(),
                viewport,
            })
            .unwrap();

        let source = load_oriented(&source_path).unwrap();
        let source = to_working_linear(&source, &engine.srgb_profile).unwrap();
        let adjusted = apply_operations(&source, &operations).unwrap();
        let (visible, _) = render_viewport(&adjusted, viewport).unwrap();
        let display = from_working_linear(&visible).unwrap();
        let rgba = ensure_rgba8(&display).unwrap();
        let reference = fit_to_canvas(&rgba, viewport).unwrap().write_to_memory();

        assert_eq!(cached.rgba8.len(), reference.len());
        let differences = cached
            .rgba8
            .iter()
            .zip(reference)
            .map(|(cached, reference)| cached.abs_diff(reference))
            .collect::<Vec<_>>();
        let maximum_difference = differences.iter().copied().max().unwrap();
        let mean_difference = differences
            .iter()
            .map(|difference| u64::from(*difference))
            .sum::<u64>() as f64
            / differences.len() as f64;
        let large_differences = differences
            .iter()
            .filter(|difference| **difference > 2)
            .count();
        // The optimized fitted preview processes an oversampled linear-light
        // proxy, so interpolation can differ slightly from the full-resolution
        // path. Keep the displayed result numerically close to that reference.
        assert!(
            mean_difference <= 2.0 && large_differences <= differences.len() / 20,
            "proxy preview differed from the full-resolution reference by {maximum_difference}; mean {mean_difference}; {large_differences} channels exceeded 2"
        );
    }

    fn apply_test_matrix(matrix: [[f32; 3]; 3], value: [f32; 3]) -> [f32; 3] {
        matrix.map(|row| row[0] * value[0] + row[1] * value[1] + row[2] * value[2])
    }

    fn channel_range(channels: &[u8]) -> u8 {
        channels.iter().max().unwrap() - channels.iter().min().unwrap()
    }

    fn expected_srgb_exposure(sample: u8, ev: f64) -> u8 {
        let linear = decode_srgb(sample);
        let exposed = linear * 2_f64.powf(ev);
        encode_srgb(exposed)
    }

    fn expected_srgb_curve(sample: u8, curve: ToneCurve) -> u8 {
        encode_srgb(f64::from(curve.sample(decode_srgb(sample) as f32)))
    }

    fn decode_srgb(sample: u8) -> f64 {
        let encoded = f64::from(sample) / 255.0;
        if encoded <= 0.040_45 {
            encoded / 12.92
        } else {
            ((encoded + 0.055) / 1.055).powf(2.4)
        }
    }

    fn encode_srgb(linear: f64) -> u8 {
        let encoded = if linear <= 0.003_130_8 {
            12.92 * linear
        } else {
            1.055 * linear.powf(1.0 / 2.4) - 0.055
        };
        (encoded.clamp(0.0, 1.0) * 255.0).round() as u8
    }

    /// Build a flat, uniform gray image with an optional alpha channel in
    /// linear scRGB space (values are already linear, not gamma-encoded).
    fn make_uniform_linear_image(
        width: i32,
        height: i32,
        value: f32,
        with_alpha: bool,
    ) -> VipsImage {
        let bands = if with_alpha { 4 } else { 3 };
        let mut data = Vec::with_capacity((width * height * bands) as usize * 4);
        for _ in 0..(width * height) {
            for _ in 0..3 {
                data.extend_from_slice(&value.to_ne_bytes());
            }
            if with_alpha {
                data.extend_from_slice(&0.5_f32.to_ne_bytes());
            }
        }
        VipsImage::new_from_memory_copy(&data, width, height, bands, ops::BandFormat::Float)
            .unwrap()
    }

    #[test]
    fn vignette_darkens_corners_more_than_center() {
        // A 100×100 uniform image. At full strength the center should be barely
        // touched while the far corners (distance ≈ 0.71 * sqrt(2) ≈ 1.0) fall
        // solidly inside the smoothstep falloff.
        let source_image = make_uniform_linear_image(100, 100, 0.5, false);

        // Convert to OKLab (as the pipeline does) and apply vignette.
        let oklab = linear_rgb_to_oklab(&source_image).unwrap();
        let vignetted_oklab = apply_vignette_on_oklab(&oklab, 1.0, 100, 100).unwrap();
        let vignetted = oklab_to_linear_rgb(&vignetted_oklab).unwrap();

        // Read the resulting float pixels.
        fn read_pixel(image: &VipsImage, x: i32, y: i32) -> f32 {
            let pixel_bytes = image.write_to_memory();
            let bands = image.get_bands() as usize;
            let idx = (y as usize * image.get_width() as usize + x as usize) * bands;
            f32::from_ne_bytes(pixel_bytes[idx * 4..idx * 4 + 4].try_into().unwrap())
        }

        let center_value = read_pixel(&vignetted, 50, 50);
        let corner_value = read_pixel(&vignetted, 0, 0);

        assert!(
            corner_value < center_value,
            "corner ({corner_value}) must be darker than center ({center_value}) with strength=1"
        );
        // Center should be essentially untouched (distance ≈ 0 → alpha ≈ 0).
        assert!(
            (center_value - 0.5).abs() < 0.02,
            "center should remain close to input at strength=1, got {center_value}"
        );
    }

    #[test]
    fn vignette_strength_zero_is_identity() {
        let source_image = make_uniform_linear_image(32, 32, 0.4, false);
        let oklab = linear_rgb_to_oklab(&source_image).unwrap();
        let vignetted_oklab = apply_vignette_on_oklab(&oklab, 0.0, 32, 32).unwrap();
        let vignetted = oklab_to_linear_rgb(&vignetted_oklab).unwrap();

        let original_bytes = source_image.write_to_memory();
        let vignetted_bytes = vignetted.write_to_memory();
        assert_eq!(
            original_bytes.len(),
            vignetted_bytes.len(),
            "image size must not change"
        );
        for (original, vignetted) in original_bytes
            .chunks_exact(4)
            .zip(vignetted_bytes.chunks_exact(4))
        {
            let orig = f32::from_ne_bytes(original.try_into().unwrap());
            let vign = f32::from_ne_bytes(vignetted.try_into().unwrap());
            assert!(
                (orig - vign).abs() < 1e-4,
                "strength=0 must not change any pixel value; orig={orig}, vign={vign}"
            );
        }
    }

    #[test]
    fn vignette_preserves_alpha_channel() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("vignette-alpha.png");

        // A 5×5 image with a semi-transparent alpha channel.
        let mut pixels = Vec::new();
        for _ in 0..25 {
            pixels.extend_from_slice(&[180_u8, 180, 180, 128]);
        }
        let image =
            VipsImage::new_from_memory_copy(&pixels, 5, 5, 4, ops::BandFormat::Uchar).unwrap();
        ops::pngsave(&image, source_path.to_str().unwrap()).unwrap();

        let mut engine = VipsEngine::new().unwrap();
        let result = engine
            .render_preview(&PreviewRequest {
                generation: 1,
                source_path,
                operations: vec![Operation::Vignette { strength: 0.8 }],
                viewport: Viewport::fit(5, 5),
            })
            .unwrap();

        // Every fourth byte is the alpha channel; it must equal 128 (≈ 50%).
        for (i, chunk) in result.rgba8.chunks_exact(4).enumerate() {
            assert_eq!(
                chunk[3], 128,
                "vignette must not alter alpha at pixel {i}: got {}",
                chunk[3]
            );
        }
    }

    #[test]
    fn vignette_does_not_shift_hue_of_neutral_gray() {
        // A neutral gray has OKLab a=0 b=0. After vignette, only L changes; a
        // and b should stay at zero so no color cast appears in the shadows.
        let source_image = make_uniform_linear_image(11, 11, 0.18, false);
        let oklab_before = linear_rgb_to_oklab(&source_image).unwrap();
        let oklab_after = apply_vignette_on_oklab(&oklab_before, 1.0, 11, 11).unwrap();

        let before_bytes = oklab_before.write_to_memory();
        let after_bytes = oklab_after.write_to_memory();
        let bands = oklab_after.get_bands() as usize;

        for (before_chunk, after_chunk) in before_bytes
            .chunks_exact(4 * bands)
            .zip(after_bytes.chunks_exact(4 * bands))
        {
            // Bands 1 and 2 are OKLab a and b (chroma).
            let a_before = f32::from_ne_bytes(before_chunk[4..8].try_into().unwrap());
            let b_before = f32::from_ne_bytes(before_chunk[8..12].try_into().unwrap());
            let a_after = f32::from_ne_bytes(after_chunk[4..8].try_into().unwrap());
            let b_after = f32::from_ne_bytes(after_chunk[8..12].try_into().unwrap());

            assert!(
                (a_before - a_after).abs() < 1e-5,
                "vignette must not alter OKLab 'a' channel; before={a_before}, after={a_after}"
            );
            assert!(
                (b_before - b_after).abs() < 1e-5,
                "vignette must not alter OKLab 'b' channel; before={b_before}, after={b_after}"
            );
        }
    }
}
