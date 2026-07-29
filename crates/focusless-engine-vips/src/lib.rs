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
    ToneCurve, Viewport, WhiteBalance,
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
    let saturation = operations
        .iter()
        .rev()
        .find_map(|operation| match *operation {
            Operation::Saturation { amount } => Some(amount),
            _ => None,
        })
        .unwrap_or(0.0);
    if saturation.abs() > f32::EPSILON {
        current = apply_saturation_oklab(&current, saturation)?;
    }
    let sharpness = operations
        .iter()
        .rev()
        .find_map(|operation| match *operation {
            Operation::Sharpness { amount } => Some(amount),
            _ => None,
        })
        .unwrap_or(0.0);
    if sharpness > f32::EPSILON {
        current = apply_sharpness_oklab(&current, sharpness)?;
    }
    Ok(current)
}

fn apply_white_balance_linear(
    image: &VipsImage,
    adjustment: WhiteBalance,
) -> Result<VipsImage, RenderError> {
    let bands = image.get_bands();
    let has_alpha = image.image_hasalpha();
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

fn apply_saturation_oklab(image: &VipsImage, amount: f32) -> Result<VipsImage, RenderError> {
    let (color, alpha) = split_color_and_alpha(image, "saturation")?;
    let oklab = linear_rgb_to_oklab(&color)?;
    let chroma_scale = 1.0 + f64::from(amount) / 100.0;
    let adjusted_oklab = ops::linear(
        &oklab,
        &mut [1.0, chroma_scale, chroma_scale],
        &mut [0.0, 0.0, 0.0],
    )
    .map_err(vips_error)?;
    let saturated = oklab_to_linear_rgb(&adjusted_oklab)?;
    join_alpha(saturated, alpha)
}

fn apply_sharpness_oklab(image: &VipsImage, amount: f32) -> Result<VipsImage, RenderError> {
    const RADIUS_SIGMA: f64 = 1.0;
    const DETAIL_THRESHOLD: f64 = 0.003;

    let (color, alpha) = split_color_and_alpha(image, "sharpness")?;
    let oklab = linear_rgb_to_oklab(&color)?;
    let lightness = ops::extract_band(&oklab, 0).map_err(vips_error)?;
    let chroma = ops::extract_band_with_opts(&oklab, 1, &ops::ExtractBandOptions { n: 2 })
        .map_err(vips_error)?;
    let blurred = ops::gaussblur(&lightness, RADIUS_SIGMA).map_err(vips_error)?;
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
    let adjusted_oklab = ops::bandjoin(&mut [adjusted_lightness, chroma]).map_err(vips_error)?;
    let sharpened = oklab_to_linear_rgb(&adjusted_oklab)?;
    join_alpha(sharpened, alpha)
}

fn split_color_and_alpha(
    image: &VipsImage,
    operation_name: &str,
) -> Result<(VipsImage, Option<VipsImage>), RenderError> {
    let bands = image.get_bands();
    let has_alpha = image.image_hasalpha();
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
    (4.0 * x / denominator, 9.0 * y / denominator)
}

fn uv_to_xy((u, v): (f64, f64)) -> (f64, f64) {
    let denominator = 6.0 * u - 16.0 * v + 12.0;
    (9.0 * u / denominator, 4.0 * v / denominator)
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
                operations: vec![Operation::Sharpness { amount: 300.0 }],
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
}
