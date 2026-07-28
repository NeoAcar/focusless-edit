//! libvips-backed rendering for Focusless Edit.
//!
//! All libvips objects are created and dropped on one dedicated worker thread.
//! This keeps the UI responsive and respects the Rust binding's thread-safety
//! constraints while libvips still uses its own internal worker pool.

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
    CropRect, ExportFormat, ExportRequest, Operation, PreviewRequest, RenderError, RenderResult,
    ToneCurve, Viewport,
};
use libvips::{VipsApp, VipsImage, ops};
use tracing::{debug, error};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug)]
pub enum EngineCommand {
    Inspect { path: PathBuf },
    Export(ExportRequest),
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
    let engine = match VipsEngine::new() {
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
        Ok(Self { app, srgb_profile })
    }

    fn inspect(&self, path: &Path) -> Result<ImageInfo, RenderError> {
        let source = load_oriented(path)?;
        dimensions(&source)
    }

    fn render_preview(&self, request: &PreviewRequest) -> Result<RenderResult, RenderError> {
        validate_viewport(request.viewport)?;
        let source = load_oriented(&request.source_path)?;
        let source = to_working_linear(&source, &self.srgb_profile)?;
        let adjusted = apply_operations(&source, &request.operations)?;
        let (visible, effective_zoom) = render_viewport(&adjusted, request.viewport)?;
        let display = from_working_linear(&visible)?;
        let rgba = ensure_rgba8(&display)?;
        let canvas = fit_to_canvas(&rgba, request.viewport)?;
        let info = dimensions(&canvas)?;
        let rgba8 = canvas.image_write_to_memory();
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

        let result = match request.format {
            ExportFormat::Jpeg { quality } => {
                let flattened = ops::flatten_with_opts(
                    &adjusted,
                    &ops::FlattenOptions {
                        background: vec![255.0, 255.0, 255.0],
                        ..Default::default()
                    },
                )
                .map_err(vips_error)?;
                ops::jpegsave_with_opts(
                    &flattened,
                    &temporary_str,
                    &ops::JpegsaveOptions {
                        q: i32::from(quality.clamp(1, 100)),
                        optimize_coding: true,
                        interlace: true,
                        keep: ops::ForeignKeep::None,
                        profile: Some(profile_string(&self.srgb_profile)?),
                        ..Default::default()
                    },
                )
            }
            ExportFormat::Png => ops::pngsave_with_opts(
                &adjusted,
                &temporary_str,
                &ops::PngsaveOptions {
                    compression: 6,
                    keep: ops::ForeignKeep::None,
                    profile: Some(profile_string(&self.srgb_profile)?),
                    ..Default::default()
                },
            ),
            // The generated Rust options target a newer libvips than Ubuntu
            // 24.04 and include fields 8.15 does not know. Filename options
            // let libvips negotiate the supported WebP surface itself.
            ExportFormat::WebP { quality } => adjusted.image_write_to_file(&format!(
                "{temporary_str}[Q={},keep=none,profile={}]",
                quality.clamp(1, 100),
                profile_string(&self.srgb_profile)?
            )),
        };
        if let Err(error) = result {
            let detail = self.app.error_buffer().unwrap_or("no libvips details");
            let message = format!("{error}: {}", detail.trim());
            self.app.error_clear();
            let _ = fs::remove_file(&temporary);
            return Err(RenderError::Engine(message));
        }

        if cancelled.load(Ordering::Acquire) {
            let _ = fs::remove_file(&temporary);
            return Err(RenderError::Cancelled);
        }
        fs::rename(&temporary, &request.destination_path)?;
        Ok(())
    }

    #[allow(dead_code)]
    fn version(&self) -> &str {
        self.app.version_string().unwrap_or("unknown")
    }
}

fn load_oriented(path: &Path) -> Result<VipsImage, RenderError> {
    let path = path_string(path)?;
    let source = VipsImage::new_from_file(&path).map_err(vips_error)?;
    ops::autorot(&source).map_err(vips_error)
}

fn resolve_srgb_profile() -> Result<PathBuf, RenderError> {
    let override_path = env::var_os("FOCUSLESS_SRGB_PROFILE").map(PathBuf::from);
    let candidates = override_path.into_iter().chain(
        [
            "/usr/share/color/icc/sRGB.icc",
            "/usr/share/color/icc/ghostscript/srgb.icc",
            "/usr/share/color/icc/colord/sRGB.icc",
        ]
        .into_iter()
        .map(PathBuf::from),
    );
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            RenderError::Engine(
                "an sRGB ICC profile is required; install icc-profiles-free or set \
                 FOCUSLESS_SRGB_PROFILE"
                    .into(),
            )
        })
}

fn profile_string(path: &Path) -> Result<String, RenderError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| RenderError::Engine("sRGB ICC profile path is not valid UTF-8".into()))
}

fn to_working_linear(image: &VipsImage, srgb_profile: &Path) -> Result<VipsImage, RenderError> {
    let has_alpha = image.image_hasalpha();
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
            ..Default::default()
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
    let has_alpha = image.image_hasalpha();
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
    Ok(current)
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
    let has_alpha = image.image_hasalpha();
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
    let has_alpha = image.image_hasalpha();
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

fn vips_error(error: libvips::error::Error) -> RenderError {
    RenderError::Engine(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_exposure_and_exports_all_supported_formats() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.png");
        let engine = VipsEngine::new().unwrap();

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

        let darker = engine
            .render_preview(&PreviewRequest {
                generation: 8,
                source_path: source_path.clone(),
                operations: vec![Operation::Exposure { ev: -1.0 }],
                viewport: Viewport::fit(2, 2),
            })
            .unwrap();
        assert!(
            (i16::from(darker.rgba8[0]) - i16::from(expected_srgb_exposure(64, -1.0))).abs() <= 1
        );
        assert_eq!(darker.rgba8[7], 128, "negative EV must preserve alpha");

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
                            Operation::Exposure { ev: 0.5 },
                        ],
                        format,
                    },
                    &AtomicBool::new(false),
                )
                .unwrap();
            assert!(destination.metadata().unwrap().len() > 0);
            let exported = VipsImage::new_from_file(destination.to_str().unwrap()).unwrap();
            assert!(
                exported.get_as_string("icc-profile-data").is_ok(),
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
}
