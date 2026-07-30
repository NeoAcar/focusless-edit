use std::{
    cell::RefCell,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use focusless_core::{
    CropRect, ExportFormat, ExportRequest, Operation, PreviewRequest, ProjectDocument,
    SourceReference, ToneCurve, ViewState, Viewport, WhiteBalance,
};
use focusless_engine_vips::{EngineEvent, EngineWorker, ImageInfo};
use focusless_storage::{SourceStatus, fingerprint_source, inspect_source, load_project};
use rfd::FileDialog;
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode};
use tracing::{error, info, warn};

use crate::{
    AppWindow,
    storage_worker::{SaveEvent, StorageWorker},
};

const AUTOSAVE_DELAY: Duration = Duration::from_millis(500);
const UI_TICK: Duration = Duration::from_millis(16);
const MIN_CANVAS_SIZE: u32 = 64;
const MAX_CANVAS_SIZE: u32 = 4096;
const MIN_CROP_EXTENT: f32 = 0.01;
const MIN_CURVE_INPUT_GAP: f32 = 0.001;

enum PendingOpen {
    Image(PathBuf),
    Project {
        path: Option<PathBuf>,
        document: ProjectDocument,
    },
}

pub struct Controller {
    engine: EngineWorker,
    storage: StorageWorker,
    document: Option<ProjectDocument>,
    project_path: Option<PathBuf>,
    pending_open: Option<PendingOpen>,
    image_info: Option<ImageInfo>,
    recovery_path: PathBuf,
    generation: u64,
    newest_generation: u64,
    effective_zoom: f32,
    exposure_edit_start: Option<f32>,
    white_balance_edit_start: Option<WhiteBalance>,
    saturation_edit_start: Option<f32>,
    sharpness_edit_start: Option<f32>,
    crop_edit_start: Option<CropRect>,
    crop_saved_view: Option<ViewState>,
    crop_aspect_ratio: Option<f32>,
    curve_edit_start: Option<ToneCurve>,
    autosave_due: Option<Instant>,
    last_canvas_size: (u32, u32),
    exporting: bool,
    _timer: Option<Timer>,
}

impl Controller {
    pub fn install(ui: &AppWindow, project_dirs: &ProjectDirs) -> Result<Rc<RefCell<Self>>> {
        let recovery_dir = project_dirs.data_local_dir().join("recovery");
        fs::create_dir_all(&recovery_dir).with_context(|| {
            format!(
                "could not create recovery directory {}",
                recovery_dir.display()
            )
        })?;

        let controller = Rc::new(RefCell::new(Self {
            engine: EngineWorker::start(),
            storage: StorageWorker::start(),
            document: None,
            project_path: None,
            pending_open: None,
            image_info: None,
            recovery_path: recovery_dir.join("untitled.focusless"),
            generation: 0,
            newest_generation: 0,
            effective_zoom: 1.0,
            exposure_edit_start: None,
            white_balance_edit_start: None,
            saturation_edit_start: None,
            sharpness_edit_start: None,
            crop_edit_start: None,
            crop_saved_view: None,
            crop_aspect_ratio: None,
            curve_edit_start: None,
            autosave_due: None,
            last_canvas_size: (0, 0),
            exporting: false,
            _timer: None,
        }));

        Self::wire_callbacks(&controller, ui);
        Self::start_tick(&controller, ui);
        Ok(controller)
    }

    #[must_use]
    pub fn recovery_path(&self) -> &Path {
        &self.recovery_path
    }

    fn wire_callbacks(controller: &Rc<RefCell<Self>>, ui: &AppWindow) {
        let weak_ui = ui.as_weak();

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_open_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                let file = FileDialog::new()
                    .set_title("Import a photo or Focusless project")
                    .add_filter(
                        "Supported files",
                        &["jpg", "jpeg", "png", "webp", "focusless"],
                    )
                    .add_filter("Photos", &["jpg", "jpeg", "png", "webp"])
                    .add_filter("Focusless project", &["focusless"])
                    .pick_file();
                if let Some(path) = file {
                    controller.borrow_mut().begin_open(&ui, path);
                } else if !file_dialog_backend_available() {
                    controller.borrow().show_error_text(
                        &ui,
                        "File dialog is unavailable",
                        "Install Zenity with: sudo apt install -y zenity",
                    );
                }
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_save_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().save(&ui, false);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_save_as_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().save(&ui, true);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_export_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().export(&ui);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_cancel_export_requested(move || {
                controller.borrow().engine.cancel_export();
                if let Some(ui) = weak_ui.upgrade() {
                    ui.set_status_text("Cancelling export…".into());
                }
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_white_balance_preview(move |temperature, tint| {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().preview_white_balance(
                    &ui,
                    WhiteBalance {
                        temperature: temperature.clamp(-100.0, 100.0),
                        tint: tint.clamp(-100.0, 100.0),
                    },
                );
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_white_balance_commit(move |temperature, tint| {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().commit_white_balance(
                    &ui,
                    WhiteBalance {
                        temperature: temperature.clamp(-100.0, 100.0),
                        tint: tint.clamp(-100.0, 100.0),
                    },
                );
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_reset_white_balance_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().reset_white_balance(&ui);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_saturation_preview(move |amount| {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller
                    .borrow_mut()
                    .preview_saturation(&ui, amount.clamp(-100.0, 100.0));
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_saturation_commit(move |amount| {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller
                    .borrow_mut()
                    .commit_saturation(&ui, amount.clamp(-100.0, 100.0));
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_reset_saturation_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().reset_saturation(&ui);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_sharpness_preview(move |amount| {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller
                    .borrow_mut()
                    .preview_sharpness(&ui, amount.clamp(0.0, 300.0));
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_sharpness_commit(move |amount| {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller
                    .borrow_mut()
                    .commit_sharpness(&ui, amount.clamp(0.0, 300.0));
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_reset_sharpness_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().reset_sharpness(&ui);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_exposure_preview(move |value| {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller
                    .borrow_mut()
                    .preview_exposure(&ui, value.clamp(-5.0, 5.0));
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_exposure_commit(move |value| {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller
                    .borrow_mut()
                    .commit_exposure(&ui, value.clamp(-5.0, 5.0));
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_reset_exposure_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().reset_exposure(&ui);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_curve_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().start_curve(&ui);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_curve_preview(move |point, input, value| {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().preview_curve(
                    &ui,
                    point,
                    input.clamp(0.0, 1.0),
                    value.clamp(0.0, 1.0),
                );
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_curve_reset_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().reset_curve(&ui);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_curve_commit_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().commit_curve_change(&ui);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_curve_done_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().finish_curve(&ui);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_rotate_left_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().rotate(&ui, -1);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_rotate_right_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().rotate(&ui, 1);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_crop_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().start_crop(&ui);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_crop_gesture(move |kind, x, y, width, height, delta_x, delta_y| {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller
                    .borrow_mut()
                    .update_crop_gesture(&ui, kind, x, y, width, height, delta_x, delta_y);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_crop_aspect_requested(move |ratio| {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().set_crop_aspect(&ui, ratio);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_crop_apply_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().apply_crop(&ui);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_crop_cancel_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().cancel_crop(&ui);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_undo_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().undo(&ui);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_redo_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().redo(&ui);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_fit_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().set_zoom(&ui, 0.0);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_actual_size_requested(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().set_zoom(&ui, 1.0);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_zoom_requested(move |factor| {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().zoom_by(&ui, factor);
            });
        }

        {
            let controller = Rc::clone(controller);
            let weak_ui = weak_ui.clone();
            ui.on_pan_requested(move |delta_x, delta_y| {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };
                controller.borrow_mut().pan(&ui, delta_x, delta_y);
            });
        }
    }

    fn start_tick(controller: &Rc<RefCell<Self>>, ui: &AppWindow) {
        let timer = Timer::default();
        let weak_controller = Rc::downgrade(controller);
        let weak_ui = ui.as_weak();
        timer.start(TimerMode::Repeated, UI_TICK, move || {
            let (Some(controller), Some(ui)) = (weak_controller.upgrade(), weak_ui.upgrade())
            else {
                return;
            };
            controller.borrow_mut().tick(&ui);
        });
        controller.borrow_mut()._timer = Some(timer);
    }

    pub fn begin_open(&mut self, ui: &AppWindow, path: PathBuf) {
        if let (Some(document), Some(original)) =
            (self.document.as_mut(), self.crop_edit_start.take())
        {
            let _ = document.preview_crop(original);
        }
        self.crop_saved_view = None;
        self.crop_aspect_ratio = None;
        ui.set_crop_mode(false);
        if let (Some(document), Some(original)) =
            (self.document.as_mut(), self.curve_edit_start.take())
        {
            let _ = document.preview_tone_curve(original);
        }
        ui.set_curve_mode(false);
        self.flush_autosave();
        ui.set_status_text(format!("Opening {}…", path.display()).into());
        ui.set_rendering(true);

        if has_extension(&path, "focusless") {
            match load_project(&path) {
                Ok(mut document) => {
                    if let Err(error) = document.validate() {
                        self.show_error(ui, "Could not open project", &error);
                        return;
                    }
                    if !document.source.path.is_file() {
                        let replacement = FileDialog::new()
                            .set_title("Locate the source photo")
                            .add_filter("Photos", &["jpg", "jpeg", "png", "webp"])
                            .pick_file();
                        let Some(replacement) = replacement else {
                            self.show_error_text(
                                ui,
                                "Source photo was not found",
                                &document.source.path.display().to_string(),
                            );
                            return;
                        };
                        document.source.path = replacement;
                    }
                    let source_path = document.source.path.clone();
                    let project_path = (path != self.recovery_path).then_some(path);
                    self.pending_open = Some(PendingOpen::Project {
                        path: project_path,
                        document,
                    });
                    self.engine.inspect(source_path);
                }
                Err(error) => self.show_error(ui, "Could not open project", &error),
            }
        } else {
            self.pending_open = Some(PendingOpen::Image(path.clone()));
            self.engine.inspect(path);
        }
    }

    fn tick(&mut self, ui: &AppWindow) {
        while let Some(event) = self.engine.try_event() {
            self.handle_engine_event(ui, event);
        }
        while let Some(event) = self.storage.try_event() {
            self.handle_save_event(ui, event);
        }

        if self.document.is_some() {
            let canvas_size = self.canvas_size(ui);
            if canvas_size != self.last_canvas_size
                && canvas_size.0 >= MIN_CANVAS_SIZE
                && canvas_size.1 >= MIN_CANVAS_SIZE
            {
                self.queue_preview(ui);
            }
        }

        if self.autosave_due.is_some_and(|due| Instant::now() >= due) {
            self.autosave_due = None;
            self.flush_autosave();
        }
    }

    fn handle_engine_event(&mut self, ui: &AppWindow, event: EngineEvent) {
        match event {
            EngineEvent::Fatal(error) => {
                self.show_error(ui, "Could not start the image engine", &error);
            }
            EngineEvent::Inspected { path, result } => match result {
                Ok(info) => self.finish_open(ui, path, info),
                Err(error) => self.show_error(ui, "Could not open photo", &error),
            },
            EngineEvent::PreviewReady(Ok(result)) => {
                if result.generation != self.newest_generation {
                    return;
                }
                let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                    &result.rgba8,
                    result.width,
                    result.height,
                );
                ui.set_preview_image(Image::from_rgba8(buffer));
                ui.set_rendering(false);
                self.effective_zoom = result.effective_zoom;
                self.update_zoom_text(ui);
                self.update_crop_overlay_geometry(ui);
                if !self.exporting {
                    ui.set_status_text("Ready".into());
                }
            }
            EngineEvent::PreviewReady(Err(error)) => {
                ui.set_rendering(false);
                self.show_error(ui, "Could not render preview", &error);
            }
            EngineEvent::ExportStarted { destination } => {
                self.exporting = true;
                ui.set_exporting(true);
                ui.set_status_text(format!("Exporting to {}…", destination.display()).into());
            }
            EngineEvent::ExportFinished {
                destination,
                result,
            } => {
                self.exporting = false;
                ui.set_exporting(false);
                match result {
                    Ok(()) => {
                        info!(path = %destination.display(), "export completed");
                        ui.set_status_text(format!("Exported to {}", destination.display()).into());
                    }
                    Err(error) => self.show_error(ui, "Export failed", &error),
                }
            }
        }
    }

    fn handle_save_event(&mut self, ui: &AppWindow, event: SaveEvent) {
        match event {
            SaveEvent::AutosaveCompleted { path, result } => match result {
                Ok(()) => info!(path = %path.display(), "autosave completed"),
                Err(error) => error!(%error, path = %path.display(), "autosave failed"),
            },
            SaveEvent::ManualSaveCompleted {
                path,
                previous_project_path,
                result,
            } => match result {
                Ok(()) => {
                    let _ = fs::remove_file(&self.recovery_path);
                    ui.set_status_text(format!("Project saved to {}", path.display()).into());
                    info!(path = %path.display(), "project saved");
                }
                Err(error) => {
                    if self.project_path.as_ref() == Some(&path) {
                        self.project_path = previous_project_path;
                    }
                    self.show_error(ui, "Could not save project", &error);
                }
            },
        }
    }

    fn finish_open(&mut self, ui: &AppWindow, inspected_path: PathBuf, info: ImageInfo) {
        let Some(pending) = self.pending_open.take() else {
            return;
        };

        let (mut document, project_path, status) = match pending {
            PendingOpen::Image(path) => {
                if path != inspected_path {
                    return;
                }
                let fingerprint = match fingerprint_source(&path, info.width, info.height) {
                    Ok(fingerprint) => fingerprint,
                    Err(error) => {
                        self.show_error(ui, "Could not verify source photo", &error);
                        return;
                    }
                };
                (
                    ProjectDocument::new(SourceReference { path, fingerprint }),
                    None,
                    "New non-destructive document",
                )
            }
            PendingOpen::Project { path, mut document } => {
                if document.source.path != inspected_path {
                    return;
                }
                let status = match inspect_source(&document.source, info.width, info.height) {
                    Ok(SourceStatus::Current) => "Project opened",
                    Ok(SourceStatus::Changed { actual, .. }) => {
                        warn!(path = %document.source.path.display(), "source image changed");
                        document.source.fingerprint = actual;
                        "The source photo changed; using the new version"
                    }
                    Ok(SourceStatus::Missing) => "Source photo was not found",
                    Err(error) => {
                        self.show_error(ui, "Could not verify source photo", &error);
                        return;
                    }
                };
                (document, path, status)
            }
        };

        document.view.center_x = document.view.center_x.clamp(0.0, 1.0);
        document.view.center_y = document.view.center_y.clamp(0.0, 1.0);
        self.document = Some(document);
        self.project_path = project_path;
        self.image_info = Some(info);
        self.exposure_edit_start = None;
        self.white_balance_edit_start = None;
        self.saturation_edit_start = None;
        self.sharpness_edit_start = None;
        self.crop_edit_start = None;
        self.crop_saved_view = None;
        self.crop_aspect_ratio = None;
        self.curve_edit_start = None;
        self.last_canvas_size = (0, 0);
        self.autosave_due = None;

        let document = self.document.as_ref().expect("document was just assigned");
        ui.set_document_loaded(true);
        ui.set_file_name(
            document
                .source
                .path
                .file_name()
                .unwrap_or_else(|| OsStr::new("Photo"))
                .to_string_lossy()
                .to_string()
                .into(),
        );
        ui.set_exposure(document.exposure_ev());
        ui.set_exposure_text(format_exposure(document.exposure_ev()).into());
        self.set_white_balance_ui(ui, document.white_balance());
        ui.set_saturation(document.saturation());
        ui.set_saturation_text(format_adjustment(document.saturation()).into());
        ui.set_sharpness(document.sharpness());
        ui.set_sharpness_text(format_nonnegative_adjustment(document.sharpness()).into());
        ui.set_crop_mode(false);
        ui.set_curve_mode(false);
        self.set_curve_ui(ui, document.tone_curve());
        self.update_transform_ui(ui);
        self.update_image_info(ui);
        ui.set_status_text(status.into());
        let opened_source = document.source.path.clone();
        self.update_history_ui(ui);
        self.queue_preview(ui);
        info!(path = %opened_source.display(), "document opened");
    }

    fn preview_white_balance(&mut self, ui: &AppWindow, adjustment: WhiteBalance) {
        if self.crop_edit_start.is_some() || self.curve_edit_start.is_some() {
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        if self.white_balance_edit_start.is_none() {
            self.white_balance_edit_start = Some(document.white_balance());
        }
        if let Err(error) = document.preview_white_balance(adjustment) {
            self.show_error(ui, "Could not apply white balance", &error);
            return;
        }
        self.set_white_balance_ui(ui, adjustment);
        self.mark_changed();
        self.queue_preview(ui);
    }

    fn commit_white_balance(&mut self, ui: &AppWindow, adjustment: WhiteBalance) {
        if self.crop_edit_start.is_some() || self.curve_edit_start.is_some() {
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        let before = self
            .white_balance_edit_start
            .take()
            .unwrap_or_else(|| document.white_balance());
        if let Err(error) = document.commit_white_balance(before, adjustment) {
            self.show_error(ui, "Could not commit white balance", &error);
            return;
        }
        self.set_white_balance_ui(ui, adjustment);
        self.mark_changed();
        self.update_history_ui(ui);
    }

    fn reset_white_balance(&mut self, ui: &AppWindow) {
        if self.crop_edit_start.is_some() || self.curve_edit_start.is_some() {
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        let before = document.white_balance();
        if document
            .commit_white_balance(before, WhiteBalance::IDENTITY)
            .is_ok()
        {
            self.white_balance_edit_start = None;
            self.set_white_balance_ui(ui, WhiteBalance::IDENTITY);
            self.mark_changed();
            self.update_history_ui(ui);
            self.queue_preview(ui);
        }
    }

    fn preview_saturation(&mut self, ui: &AppWindow, amount: f32) {
        if self.crop_edit_start.is_some() || self.curve_edit_start.is_some() {
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        if self.saturation_edit_start.is_none() {
            self.saturation_edit_start = Some(document.saturation());
        }
        if let Err(error) = document.preview_saturation(amount) {
            self.show_error(ui, "Could not apply saturation", &error);
            return;
        }
        ui.set_saturation_text(format_adjustment(amount).into());
        self.mark_changed();
        self.queue_preview(ui);
    }

    fn commit_saturation(&mut self, ui: &AppWindow, amount: f32) {
        if self.crop_edit_start.is_some() || self.curve_edit_start.is_some() {
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        let before = self
            .saturation_edit_start
            .take()
            .unwrap_or_else(|| document.saturation());
        if let Err(error) = document.commit_saturation(before, amount) {
            self.show_error(ui, "Could not commit saturation", &error);
            return;
        }
        ui.set_saturation_text(format_adjustment(amount).into());
        self.mark_changed();
        self.update_history_ui(ui);
    }

    fn reset_saturation(&mut self, ui: &AppWindow) {
        if self.crop_edit_start.is_some() || self.curve_edit_start.is_some() {
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        let before = document.saturation();
        if document.commit_saturation(before, 0.0).is_ok() {
            self.saturation_edit_start = None;
            ui.set_saturation(0.0);
            ui.set_saturation_text(format_adjustment(0.0).into());
            self.mark_changed();
            self.update_history_ui(ui);
            self.queue_preview(ui);
        }
    }

    fn preview_sharpness(&mut self, ui: &AppWindow, amount: f32) {
        if self.crop_edit_start.is_some() || self.curve_edit_start.is_some() {
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        if self.sharpness_edit_start.is_none() {
            self.sharpness_edit_start = Some(document.sharpness());
        }
        if let Err(error) = document.preview_sharpness(amount) {
            self.show_error(ui, "Could not apply sharpness", &error);
            return;
        }
        ui.set_sharpness_text(format_nonnegative_adjustment(amount).into());
        self.mark_changed();
        self.queue_preview(ui);
    }

    fn commit_sharpness(&mut self, ui: &AppWindow, amount: f32) {
        if self.crop_edit_start.is_some() || self.curve_edit_start.is_some() {
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        let before = self
            .sharpness_edit_start
            .take()
            .unwrap_or_else(|| document.sharpness());
        if let Err(error) = document.commit_sharpness(before, amount) {
            self.show_error(ui, "Could not commit sharpness", &error);
            return;
        }
        ui.set_sharpness_text(format_nonnegative_adjustment(amount).into());
        self.mark_changed();
        self.update_history_ui(ui);
    }

    fn reset_sharpness(&mut self, ui: &AppWindow) {
        if self.crop_edit_start.is_some() || self.curve_edit_start.is_some() {
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        let before = document.sharpness();
        if document.commit_sharpness(before, 0.0).is_ok() {
            self.sharpness_edit_start = None;
            ui.set_sharpness(0.0);
            ui.set_sharpness_text(format_nonnegative_adjustment(0.0).into());
            self.mark_changed();
            self.update_history_ui(ui);
            self.queue_preview(ui);
        }
    }

    fn preview_exposure(&mut self, ui: &AppWindow, value: f32) {
        if self.curve_edit_start.is_some() {
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        if self.exposure_edit_start.is_none() {
            self.exposure_edit_start = Some(document.exposure_ev());
        }
        if let Err(error) = document.preview_exposure(value) {
            self.show_error(ui, "Could not apply exposure", &error);
            return;
        }
        ui.set_exposure_text(format_exposure(value).into());
        self.mark_changed();
        self.queue_preview(ui);
    }

    fn commit_exposure(&mut self, ui: &AppWindow, value: f32) {
        if self.curve_edit_start.is_some() {
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        let before = self
            .exposure_edit_start
            .take()
            .unwrap_or_else(|| document.exposure_ev());
        if let Err(error) = document.commit_exposure(before, value) {
            self.show_error(ui, "Could not commit exposure", &error);
            return;
        }
        self.mark_changed();
        self.update_history_ui(ui);
    }

    fn reset_exposure(&mut self, ui: &AppWindow) {
        if self.curve_edit_start.is_some() {
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        let before = document.exposure_ev();
        if document.commit_exposure(before, 0.0).is_ok() {
            self.exposure_edit_start = None;
            ui.set_exposure(0.0);
            ui.set_exposure_text(format_exposure(0.0).into());
            self.mark_changed();
            self.update_history_ui(ui);
            self.queue_preview(ui);
        }
    }

    fn start_curve(&mut self, ui: &AppWindow) {
        if self.curve_edit_start.is_some() || self.crop_edit_start.is_some() {
            return;
        }
        self.flush_autosave();
        self.autosave_due = None;
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let curve = document.tone_curve();
        self.curve_edit_start = Some(curve);
        ui.set_curve_mode(true);
        self.set_curve_ui(ui, curve);
        ui.set_status_text("Drag any white control point in any direction".into());
        self.update_history_ui(ui);
    }

    fn preview_curve(&mut self, ui: &AppWindow, point: i32, input: f32, value: f32) {
        if self.curve_edit_start.is_none() {
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        let mut curve = document.tone_curve();
        match point {
            0 => {
                curve.shadow_input = input.clamp(
                    MIN_CURVE_INPUT_GAP,
                    curve.midtone_input - MIN_CURVE_INPUT_GAP,
                );
                curve.shadows = value;
            }
            1 => {
                curve.midtone_input = input.clamp(
                    curve.shadow_input + MIN_CURVE_INPUT_GAP,
                    curve.highlight_input - MIN_CURVE_INPUT_GAP,
                );
                curve.midtones = value;
            }
            2 => {
                curve.highlight_input = input.clamp(
                    curve.midtone_input + MIN_CURVE_INPUT_GAP,
                    1.0 - MIN_CURVE_INPUT_GAP,
                );
                curve.highlights = value;
            }
            _ => return,
        }
        if let Err(error) = document.preview_tone_curve(curve) {
            self.show_error(ui, "Could not preview tone curve", &error);
            return;
        }
        self.set_curve_ui(ui, curve);
        self.queue_preview(ui);
    }

    fn reset_curve(&mut self, ui: &AppWindow) {
        if self.curve_edit_start.is_none() {
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        if document.preview_tone_curve(ToneCurve::IDENTITY).is_ok() {
            self.set_curve_ui(ui, ToneCurve::IDENTITY);
            self.queue_preview(ui);
            self.commit_curve_change(ui);
        }
    }

    fn commit_curve_change(&mut self, ui: &AppWindow) {
        let Some(before) = self.curve_edit_start else {
            return;
        };
        let Some(document) = self.document.as_mut() else {
            return;
        };
        let after = document.tone_curve();
        if before == after {
            return;
        }
        if let Err(error) = document.commit_tone_curve(before, after) {
            self.show_error(ui, "Could not commit tone curve", &error);
            return;
        }
        self.curve_edit_start = Some(after);
        self.set_curve_ui(ui, after);
        ui.set_status_text("Tone curve updated".into());
        self.mark_changed();
        self.update_history_ui(ui);
    }

    fn finish_curve(&mut self, ui: &AppWindow) {
        if self.curve_edit_start.is_none() {
            return;
        }
        self.commit_curve_change(ui);
        self.curve_edit_start = None;
        ui.set_curve_mode(false);
        ui.set_status_text("Tone curve editor closed".into());
        self.update_history_ui(ui);
    }

    fn rotate(&mut self, ui: &AppWindow, quarter_turn_delta: i8) {
        if self.crop_edit_start.is_some() || self.curve_edit_start.is_some() {
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        if let Err(error) = document.rotate_by(quarter_turn_delta) {
            self.show_error(ui, "Could not rotate photo", &error);
            return;
        }
        document.view = ViewState::default();
        self.update_transform_ui(ui);
        self.update_image_info(ui);
        self.mark_changed();
        self.update_history_ui(ui);
        self.queue_preview(ui);
    }

    fn start_crop(&mut self, ui: &AppWindow) {
        if self.crop_edit_start.is_some() || self.curve_edit_start.is_some() {
            return;
        }
        self.flush_autosave();
        self.autosave_due = None;
        let Some(document) = self.document.as_mut() else {
            return;
        };
        let crop = document.crop_rect();
        self.crop_edit_start = Some(crop);
        self.crop_saved_view = Some(document.view);
        self.crop_aspect_ratio = None;
        document.view = ViewState::default();
        ui.set_crop_mode(true);
        ui.set_crop_aspect_mode(0);
        self.set_crop_ui(ui, crop);
        ui.set_status_text(
            "Drag inside to move the crop, or drag an edge or corner to resize".into(),
        );
        self.update_history_ui(ui);
        self.queue_preview(ui);
    }

    #[allow(clippy::too_many_arguments)]
    fn update_crop_gesture(
        &mut self,
        ui: &AppWindow,
        kind: i32,
        start_x: f32,
        start_y: f32,
        start_width: f32,
        start_height: f32,
        delta_x: f32,
        delta_y: f32,
    ) {
        if self.crop_edit_start.is_none() {
            return;
        }
        let normalized_aspect = self.normalized_crop_aspect();
        let Some(document) = self.document.as_mut() else {
            return;
        };
        let Some(rect) = crop_from_gesture(
            CropRect {
                x: start_x,
                y: start_y,
                width: start_width,
                height: start_height,
            },
            kind,
            delta_x,
            delta_y,
            normalized_aspect,
        ) else {
            return;
        };

        if document.preview_crop(rect).is_ok() {
            self.set_crop_ui(ui, rect);
        }
    }

    fn set_crop_aspect(&mut self, ui: &AppWindow, target_aspect: f32) {
        if self.crop_edit_start.is_none() {
            return;
        }
        if target_aspect < 0.0 {
            self.crop_aspect_ratio = None;
            ui.set_crop_aspect_mode(0);
            ui.set_status_text("Free crop enabled".into());
            return;
        }
        let (Some(document), Some(info)) = (self.document.as_mut(), self.image_info) else {
            return;
        };
        let rect = if target_aspect <= 0.0 {
            self.crop_aspect_ratio = None;
            ui.set_crop_aspect_mode(0);
            CropRect::FULL
        } else {
            self.crop_aspect_ratio = Some(target_aspect);
            ui.set_crop_aspect_mode(aspect_mode(target_aspect));
            let (width, height) = if document.rotation_quarter_turns().is_multiple_of(2) {
                (info.width as f32, info.height as f32)
            } else {
                (info.height as f32, info.width as f32)
            };
            let image_aspect = width / height;
            let (crop_width, crop_height) = if target_aspect <= image_aspect {
                (target_aspect / image_aspect, 1.0)
            } else {
                (1.0, image_aspect / target_aspect)
            };
            CropRect {
                x: (1.0 - crop_width) / 2.0,
                y: (1.0 - crop_height) / 2.0,
                width: crop_width,
                height: crop_height,
            }
            .normalized()
        };
        if document.preview_crop(rect).is_ok() {
            self.set_crop_ui(ui, rect);
        }
    }

    fn apply_crop(&mut self, ui: &AppWindow) {
        let Some(before) = self.crop_edit_start.take() else {
            return;
        };
        let Some(document) = self.document.as_mut() else {
            return;
        };
        let after = document.crop_rect();
        if let Err(error) = document.commit_crop(before, after) {
            self.show_error(ui, "Could not apply crop", &error);
            return;
        }
        self.crop_saved_view = None;
        self.crop_aspect_ratio = None;
        document.view = ViewState::default();
        ui.set_crop_mode(false);
        ui.set_status_text("Crop applied".into());
        self.update_transform_ui(ui);
        self.update_image_info(ui);
        self.mark_changed();
        self.update_history_ui(ui);
        self.queue_preview(ui);
    }

    fn cancel_crop(&mut self, ui: &AppWindow) {
        let Some(original) = self.crop_edit_start.take() else {
            return;
        };
        let Some(document) = self.document.as_mut() else {
            return;
        };
        let _ = document.preview_crop(original);
        self.crop_aspect_ratio = None;
        if let Some(view) = self.crop_saved_view.take() {
            document.view = view;
        }
        ui.set_crop_mode(false);
        ui.set_status_text("Crop cancelled".into());
        self.update_transform_ui(ui);
        self.update_history_ui(ui);
        self.queue_preview(ui);
    }

    fn undo(&mut self, ui: &AppWindow) {
        if self.crop_edit_start.is_some() || self.curve_edit_start.is_some() {
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        self.exposure_edit_start = None;
        self.white_balance_edit_start = None;
        self.saturation_edit_start = None;
        self.sharpness_edit_start = None;
        if document.undo() {
            let exposure = document.exposure_ev();
            let white_balance = document.white_balance();
            let saturation = document.saturation();
            let sharpness = document.sharpness();
            let curve = document.tone_curve();
            ui.set_exposure(exposure);
            ui.set_exposure_text(format_exposure(exposure).into());
            self.set_white_balance_ui(ui, white_balance);
            ui.set_saturation(saturation);
            ui.set_saturation_text(format_adjustment(saturation).into());
            ui.set_sharpness(sharpness);
            ui.set_sharpness_text(format_nonnegative_adjustment(sharpness).into());
            self.set_curve_ui(ui, curve);
            self.update_transform_ui(ui);
            self.update_image_info(ui);
            self.mark_changed();
            self.update_history_ui(ui);
            self.queue_preview(ui);
        }
    }

    fn redo(&mut self, ui: &AppWindow) {
        if self.crop_edit_start.is_some() || self.curve_edit_start.is_some() {
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        self.exposure_edit_start = None;
        self.white_balance_edit_start = None;
        self.saturation_edit_start = None;
        self.sharpness_edit_start = None;
        if document.redo() {
            let exposure = document.exposure_ev();
            let white_balance = document.white_balance();
            let saturation = document.saturation();
            let sharpness = document.sharpness();
            let curve = document.tone_curve();
            ui.set_exposure(exposure);
            ui.set_exposure_text(format_exposure(exposure).into());
            self.set_white_balance_ui(ui, white_balance);
            ui.set_saturation(saturation);
            ui.set_saturation_text(format_adjustment(saturation).into());
            ui.set_sharpness(sharpness);
            ui.set_sharpness_text(format_nonnegative_adjustment(sharpness).into());
            self.set_curve_ui(ui, curve);
            self.update_transform_ui(ui);
            self.update_image_info(ui);
            self.mark_changed();
            self.update_history_ui(ui);
            self.queue_preview(ui);
        }
    }

    fn set_zoom(&mut self, ui: &AppWindow, zoom: f32) {
        if self.crop_edit_start.is_some() || self.curve_edit_start.is_some() {
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        document.view.zoom = zoom;
        document.view.center_x = 0.5;
        document.view.center_y = 0.5;
        self.mark_changed();
        self.queue_preview(ui);
    }

    fn zoom_by(&mut self, ui: &AppWindow, factor: f32) {
        if self.crop_edit_start.is_some() || self.curve_edit_start.is_some() {
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        let starting_zoom = if document.view.zoom <= 0.0 {
            self.effective_zoom
        } else {
            document.view.zoom
        };
        document.view.zoom = (starting_zoom * factor).clamp(0.01, 32.0);
        self.mark_changed();
        self.queue_preview(ui);
    }

    fn pan(&mut self, ui: &AppWindow, delta_x: f32, delta_y: f32) {
        let (Some(document), Some(info)) = (self.document.as_mut(), self.image_info) else {
            return;
        };
        if document.view.zoom <= 0.0
            || self.crop_edit_start.is_some()
            || self.curve_edit_start.is_some()
        {
            return;
        }
        let scale_factor = ui.window().scale_factor();
        let zoom = document.view.zoom.max(0.01);
        let (image_width, image_height) = document.output_dimensions(info.width, info.height);
        document.view.center_x = (document.view.center_x
            - delta_x * scale_factor / (image_width as f32 * zoom))
            .clamp(0.0, 1.0);
        document.view.center_y = (document.view.center_y
            - delta_y * scale_factor / (image_height as f32 * zoom))
            .clamp(0.0, 1.0);
        self.mark_changed();
        self.queue_preview(ui);
    }

    fn save(&mut self, ui: &AppWindow, force_dialog: bool) {
        if self.crop_edit_start.is_some() {
            ui.set_status_text("Apply or cancel the crop before saving".into());
            return;
        }
        if self.curve_edit_start.is_some() {
            self.finish_curve(ui);
        }
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let path = if force_dialog || self.project_path.is_none() {
            let default_name = document
                .source
                .path
                .file_stem()
                .unwrap_or_else(|| OsStr::new("edit"))
                .to_string_lossy();
            FileDialog::new()
                .set_title("Save Focusless project")
                .add_filter("Focusless project", &["focusless"])
                .set_file_name(format!("{default_name}.focusless"))
                .save_file()
                .map(ensure_project_extension)
        } else {
            self.project_path.clone()
        };
        let Some(path) = path else {
            if !file_dialog_backend_available() {
                self.show_error_text(
                    ui,
                    "File dialog is unavailable",
                    "Install Zenity with: sudo apt install -y zenity",
                );
            }
            return;
        };

        let previous_project_path = self.project_path.replace(path.clone());
        self.autosave_due = None;
        ui.set_status_text(format!("Saving to {}…", path.display()).into());
        self.storage
            .save_manual(path, document.clone(), previous_project_path);
    }

    fn export(&mut self, ui: &AppWindow) {
        if self.exporting {
            return;
        }
        if self.curve_edit_start.is_some() {
            self.finish_curve(ui);
        }
        if self.crop_edit_start.is_some() {
            ui.set_status_text("Apply or cancel the crop before exporting".into());
            return;
        }
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let default_stem = document
            .source
            .path
            .file_stem()
            .unwrap_or_else(|| OsStr::new("photo"))
            .to_string_lossy();
        let destination = FileDialog::new()
            .set_title("Export edited photo")
            .add_filter("JPEG", &["jpg", "jpeg"])
            .add_filter("PNG", &["png"])
            .add_filter("WebP", &["webp"])
            .set_file_name(format!("{default_stem}-edited.jpg"))
            .save_file();
        let Some(mut destination) = destination else {
            if !file_dialog_backend_available() {
                self.show_error_text(
                    ui,
                    "File dialog is unavailable",
                    "Install Zenity with: sudo apt install -y zenity",
                );
            }
            return;
        };
        if destination.extension().is_none() {
            destination.set_extension("jpg");
        }
        let Some(format) = export_format(&destination) else {
            self.show_error_text(
                ui,
                "Unsupported output format",
                "Use a .jpg, .png, or .webp extension.",
            );
            return;
        };
        self.engine.export(ExportRequest {
            source_path: document.source.path.clone(),
            destination_path: destination,
            operations: document.operations.clone(),
            format,
        });
        self.exporting = true;
        ui.set_exporting(true);
        ui.set_status_text("Export queued…".into());
    }

    fn queue_preview(&mut self, ui: &AppWindow) {
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let (width, height) = self.canvas_size(ui);
        if width < MIN_CANVAS_SIZE || height < MIN_CANVAS_SIZE {
            return;
        }
        self.last_canvas_size = (width, height);
        self.generation = self.generation.wrapping_add(1);
        self.newest_generation = self.generation;
        ui.set_rendering(true);
        let crop_mode = self.crop_edit_start.is_some();
        let operations = if crop_mode {
            document
                .operations
                .iter()
                .copied()
                .filter(|operation| !matches!(operation, Operation::Crop { .. }))
                .collect()
        } else {
            document.operations.clone()
        };
        self.engine.request_preview(PreviewRequest {
            generation: self.generation,
            source_path: document.source.path.clone(),
            operations,
            viewport: Viewport {
                output_width: width,
                output_height: height,
                zoom: if crop_mode { 0.0 } else { document.view.zoom },
                center_x: if crop_mode {
                    0.5
                } else {
                    document.view.center_x
                },
                center_y: if crop_mode {
                    0.5
                } else {
                    document.view.center_y
                },
            },
        });
    }

    fn canvas_size(&self, ui: &AppWindow) -> (u32, u32) {
        let scale = ui.window().scale_factor();
        let width = (ui.get_canvas_width().max(0) as f32 * scale).round() as u32;
        let height = (ui.get_canvas_height().max(0) as f32 * scale).round() as u32;
        (
            width.clamp(0, MAX_CANVAS_SIZE),
            height.clamp(0, MAX_CANVAS_SIZE),
        )
    }

    fn update_history_ui(&self, ui: &AppWindow) {
        if self.crop_edit_start.is_some() || self.curve_edit_start.is_some() {
            ui.set_can_undo(false);
            ui.set_can_redo(false);
        } else if let Some(document) = &self.document {
            ui.set_can_undo(document.history.can_undo());
            ui.set_can_redo(document.history.can_redo());
        } else {
            ui.set_can_undo(false);
            ui.set_can_redo(false);
        }
    }

    fn update_zoom_text(&self, ui: &AppWindow) {
        let label = if self
            .document
            .as_ref()
            .is_some_and(|document| document.view.zoom <= 0.0)
        {
            format!("Fit · {:.0}%", self.effective_zoom * 100.0)
        } else {
            format!("{:.0}%", self.effective_zoom * 100.0)
        };
        ui.set_zoom_text(label.into());
    }

    fn set_crop_ui(&self, ui: &AppWindow, crop: CropRect) {
        ui.set_crop_x(crop.x);
        ui.set_crop_y(crop.y);
        ui.set_crop_width(crop.width);
        ui.set_crop_height(crop.height);
        ui.set_crop_text(format_crop(crop).into());
    }

    fn set_curve_ui(&self, ui: &AppWindow, curve: ToneCurve) {
        ui.set_curve_shadow_input(curve.shadow_input);
        ui.set_curve_shadows(curve.shadows);
        ui.set_curve_midtone_input(curve.midtone_input);
        ui.set_curve_midtones(curve.midtones);
        ui.set_curve_highlight_input(curve.highlight_input);
        ui.set_curve_highlights(curve.highlights);
        ui.set_curve_path(curve_path(curve).into());
        ui.set_curve_text(format_curve(curve).into());
    }

    fn set_white_balance_ui(&self, ui: &AppWindow, adjustment: WhiteBalance) {
        ui.set_temperature(adjustment.temperature);
        ui.set_tint(adjustment.tint);
        ui.set_temperature_text(format_adjustment(adjustment.temperature).into());
        ui.set_tint_text(format_adjustment(adjustment.tint).into());
    }

    fn update_transform_ui(&self, ui: &AppWindow) {
        let Some(document) = self.document.as_ref() else {
            return;
        };
        ui.set_rotation_text(
            format!("{}°", u16::from(document.rotation_quarter_turns()) * 90).into(),
        );
        self.set_crop_ui(ui, document.crop_rect());
    }

    fn update_image_info(&self, ui: &AppWindow) {
        let (Some(document), Some(info)) = (self.document.as_ref(), self.image_info) else {
            return;
        };
        let (width, height) = document.output_dimensions(info.width, info.height);
        ui.set_image_info(format!("{width} × {height} px").into());
    }

    fn update_crop_overlay_geometry(&self, ui: &AppWindow) {
        if self.crop_edit_start.is_none() {
            return;
        }
        let (Some(document), Some(info)) = (self.document.as_ref(), self.image_info) else {
            return;
        };
        let (width, height) = if document.rotation_quarter_turns().is_multiple_of(2) {
            (info.width as f32, info.height as f32)
        } else {
            (info.height as f32, info.width as f32)
        };
        let scale_factor = ui.window().scale_factor();
        let photo_width = width * self.effective_zoom / scale_factor;
        let photo_height = height * self.effective_zoom / scale_factor;
        let canvas_width = ui.get_canvas_width() as f32;
        let canvas_height = ui.get_canvas_height() as f32;
        ui.set_photo_x(((canvas_width - photo_width) / 2.0).max(0.0));
        ui.set_photo_y(((canvas_height - photo_height) / 2.0).max(0.0));
        ui.set_photo_width(photo_width.min(canvas_width));
        ui.set_photo_height(photo_height.min(canvas_height));
    }

    fn normalized_crop_aspect(&self) -> Option<f32> {
        let target_aspect = self.crop_aspect_ratio?;
        let document = self.document.as_ref()?;
        let info = self.image_info?;
        let (width, height) = if document.rotation_quarter_turns().is_multiple_of(2) {
            (info.width as f32, info.height as f32)
        } else {
            (info.height as f32, info.width as f32)
        };
        Some(target_aspect / (width / height))
    }

    fn mark_changed(&mut self) {
        self.autosave_due = Some(Instant::now() + AUTOSAVE_DELAY);
    }

    fn flush_autosave(&mut self) {
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let path = self.project_path.as_deref().unwrap_or(&self.recovery_path);
        self.storage.autosave(path.to_path_buf(), document.clone());
    }

    fn show_error(&self, ui: &AppWindow, context: &str, error: &dyn std::fmt::Display) {
        self.show_error_text(ui, context, &error.to_string());
    }

    fn show_error_text(&self, ui: &AppWindow, context: &str, detail: &str) {
        error!(context, detail, "application error");
        ui.set_rendering(false);
        ui.set_status_text(format!("{context}: {detail}").into());
    }
}

fn has_extension(path: &Path, expected: &str) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
}

#[cfg(not(target_os = "linux"))]
fn file_dialog_backend_available() -> bool {
    true
}

#[cfg(target_os = "linux")]
fn file_dialog_backend_available() -> bool {
    let zenity_available = std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join("zenity").is_file())
    });
    let data_directories =
        std::env::var_os("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let portal_available = std::env::split_paths(&data_directories).any(|directory| {
        directory
            .join("dbus-1/services/org.freedesktop.portal.Desktop.service")
            .is_file()
    });
    zenity_available || portal_available
}

fn ensure_project_extension(mut path: PathBuf) -> PathBuf {
    if !has_extension(&path, "focusless") {
        path.set_extension("focusless");
    }
    path
}

fn export_format(path: &Path) -> Option<ExportFormat> {
    let extension = path.extension()?.to_string_lossy().to_ascii_lowercase();
    match extension.as_str() {
        "jpg" | "jpeg" => Some(ExportFormat::Jpeg { quality: 92 }),
        "png" => Some(ExportFormat::Png),
        "webp" => Some(ExportFormat::WebP { quality: 90 }),
        _ => None,
    }
}

fn format_exposure(value: f32) -> String {
    if value > 0.0 {
        format!("+{value:.1} EV")
    } else {
        format!("{value:.1} EV")
    }
}

fn format_adjustment(value: f32) -> String {
    if value > 0.0 {
        format!("+{value:.0}")
    } else {
        format!("{value:.0}")
    }
}

fn format_nonnegative_adjustment(value: f32) -> String {
    format!("{value:.0}")
}

fn format_crop(crop: CropRect) -> String {
    if crop.is_full() {
        "Full image".to_owned()
    } else {
        format!("{:.0}% × {:.0}%", crop.width * 100.0, crop.height * 100.0)
    }
}

fn format_curve(curve: ToneCurve) -> String {
    if curve.is_identity() {
        "Identity".to_owned()
    } else {
        format!(
            "S {:.0}% · M {:.0}% · H {:.0}%",
            curve.shadows * 100.0,
            curve.midtones * 100.0,
            curve.highlights * 100.0
        )
    }
}

fn curve_path(curve: ToneCurve) -> String {
    let mut path = String::with_capacity(900);
    for index in 0..=64 {
        let x = index as f32 / 64.0;
        let y = 1.0 - curve.sample(x);
        if index == 0 {
            path.push_str(&format!("M {x:.5} {y:.5}"));
        } else {
            path.push_str(&format!(" L {x:.5} {y:.5}"));
        }
    }
    path
}

fn aspect_mode(aspect: f32) -> i32 {
    if (aspect - 1.0).abs() < 0.01 {
        1
    } else if (aspect - 4.0 / 3.0).abs() < 0.01 {
        2
    } else {
        3
    }
}

fn crop_from_gesture(
    start: CropRect,
    kind: i32,
    delta_x: f32,
    delta_y: f32,
    locked_aspect: Option<f32>,
) -> Option<CropRect> {
    let left = start.x;
    let top = start.y;
    let right = start.x + start.width;
    let bottom = start.y + start.height;

    if kind == 0 {
        return Some(
            CropRect {
                x: (left + delta_x).clamp(0.0, 1.0 - start.width),
                y: (top + delta_y).clamp(0.0, 1.0 - start.height),
                ..start
            }
            .normalized(),
        );
    }

    let Some(aspect) = locked_aspect else {
        let rect = match kind {
            1 => {
                let x = (left + delta_x).clamp(0.0, right - MIN_CROP_EXTENT);
                let y = (top + delta_y).clamp(0.0, bottom - MIN_CROP_EXTENT);
                CropRect {
                    x,
                    y,
                    width: right - x,
                    height: bottom - y,
                }
            }
            2 => {
                let new_right = (right + delta_x).clamp(left + MIN_CROP_EXTENT, 1.0);
                let y = (top + delta_y).clamp(0.0, bottom - MIN_CROP_EXTENT);
                CropRect {
                    x: left,
                    y,
                    width: new_right - left,
                    height: bottom - y,
                }
            }
            3 => {
                let x = (left + delta_x).clamp(0.0, right - MIN_CROP_EXTENT);
                let new_bottom = (bottom + delta_y).clamp(top + MIN_CROP_EXTENT, 1.0);
                CropRect {
                    x,
                    y: top,
                    width: right - x,
                    height: new_bottom - top,
                }
            }
            4 => {
                let new_right = (right + delta_x).clamp(left + MIN_CROP_EXTENT, 1.0);
                let new_bottom = (bottom + delta_y).clamp(top + MIN_CROP_EXTENT, 1.0);
                CropRect {
                    x: left,
                    y: top,
                    width: new_right - left,
                    height: new_bottom - top,
                }
            }
            5 => {
                let x = (left + delta_x).clamp(0.0, right - MIN_CROP_EXTENT);
                CropRect {
                    x,
                    width: right - x,
                    ..start
                }
            }
            6 => {
                let y = (top + delta_y).clamp(0.0, bottom - MIN_CROP_EXTENT);
                CropRect {
                    y,
                    height: bottom - y,
                    ..start
                }
            }
            7 => CropRect {
                width: (right + delta_x).clamp(left + MIN_CROP_EXTENT, 1.0) - left,
                ..start
            },
            8 => CropRect {
                height: (bottom + delta_y).clamp(top + MIN_CROP_EXTENT, 1.0) - top,
                ..start
            },
            _ => return None,
        };
        return Some(rect.normalized());
    };

    let aspect = aspect.max(f32::EPSILON);
    let rect = match kind {
        1..=4 => {
            let candidate_width = match kind {
                1 | 3 => right - (left + delta_x),
                _ => right + delta_x - left,
            }
            .max(MIN_CROP_EXTENT);
            let candidate_height = match kind {
                1 | 2 => bottom - (top + delta_y),
                _ => bottom + delta_y - top,
            }
            .max(MIN_CROP_EXTENT);
            let width_driven = ((candidate_width - start.width) / start.width).abs()
                >= ((candidate_height - start.height) / start.height).abs();
            let desired_width = if width_driven {
                candidate_width
            } else {
                candidate_height * aspect
            };
            let (max_width, max_height) = match kind {
                1 => (right, bottom),
                2 => (1.0 - left, bottom),
                3 => (right, 1.0 - top),
                _ => (1.0 - left, 1.0 - top),
            };
            let (width, height) =
                constrained_crop_size(desired_width, max_width, max_height, aspect);
            match kind {
                1 => CropRect {
                    x: right - width,
                    y: bottom - height,
                    width,
                    height,
                },
                2 => CropRect {
                    x: left,
                    y: bottom - height,
                    width,
                    height,
                },
                3 => CropRect {
                    x: right - width,
                    y: top,
                    width,
                    height,
                },
                _ => CropRect {
                    x: left,
                    y: top,
                    width,
                    height,
                },
            }
        }
        5 | 7 => {
            let center_y = top + start.height / 2.0;
            let desired_width = if kind == 5 {
                right - (left + delta_x)
            } else {
                right + delta_x - left
            };
            let max_width = if kind == 5 { right } else { 1.0 - left };
            let max_height = 2.0 * center_y.min(1.0 - center_y);
            let (width, height) =
                constrained_crop_size(desired_width, max_width, max_height, aspect);
            CropRect {
                x: if kind == 5 { right - width } else { left },
                y: center_y - height / 2.0,
                width,
                height,
            }
        }
        6 | 8 => {
            let center_x = left + start.width / 2.0;
            let desired_height = if kind == 6 {
                bottom - (top + delta_y)
            } else {
                bottom + delta_y - top
            };
            let max_width = 2.0 * center_x.min(1.0 - center_x);
            let max_height = if kind == 6 { bottom } else { 1.0 - top };
            let (width, height) =
                constrained_crop_size(desired_height * aspect, max_width, max_height, aspect);
            CropRect {
                x: center_x - width / 2.0,
                y: if kind == 6 { bottom - height } else { top },
                width,
                height,
            }
        }
        _ => return None,
    };
    Some(rect.normalized())
}

fn constrained_crop_size(
    desired_width: f32,
    max_width: f32,
    max_height: f32,
    aspect: f32,
) -> (f32, f32) {
    let max_allowed_width = max_width.min(max_height * aspect);
    let min_allowed_width = MIN_CROP_EXTENT
        .max(MIN_CROP_EXTENT * aspect)
        .min(max_allowed_width);
    let width = desired_width.clamp(min_allowed_width, max_allowed_width);
    (width, width / aspect)
}

#[cfg(test)]
mod crop_tests {
    use super::*;

    fn rect() -> CropRect {
        CropRect {
            x: 0.1,
            y: 0.2,
            width: 0.8,
            height: 0.6,
        }
    }

    #[test]
    fn free_crop_supports_all_four_edges() {
        let left = crop_from_gesture(rect(), 5, 0.1, 0.0, None).unwrap();
        assert!((left.x - 0.2).abs() < 0.0001);
        assert!((left.width - 0.7).abs() < 0.0001);

        let top = crop_from_gesture(rect(), 6, 0.0, 0.1, None).unwrap();
        assert!((top.y - 0.3).abs() < 0.0001);
        assert!((top.height - 0.5).abs() < 0.0001);

        let right = crop_from_gesture(rect(), 7, -0.1, 0.0, None).unwrap();
        assert!((right.width - 0.7).abs() < 0.0001);

        let bottom = crop_from_gesture(rect(), 8, 0.0, -0.1, None).unwrap();
        assert!((bottom.height - 0.5).abs() < 0.0001);
    }

    #[test]
    fn locked_edge_resize_preserves_aspect() {
        let square = CropRect {
            x: 0.2,
            y: 0.2,
            width: 0.6,
            height: 0.6,
        };
        let resized = crop_from_gesture(square, 7, -0.2, 0.0, Some(1.0)).unwrap();
        assert!((resized.width - resized.height).abs() < 0.0001);
        assert!((resized.y - 0.3).abs() < 0.0001);
    }
}
