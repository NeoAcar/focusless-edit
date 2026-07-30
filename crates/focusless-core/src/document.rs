use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROJECT_SCHEMA_VERSION: u32 = 11;
pub const MAX_HISTORY_LEN: usize = 200;
const MIN_CROP_EXTENT: f32 = 0.01;
const MAX_FRAME_WIDTH_PCT: f32 = 50.0;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFingerprint {
    pub byte_len: u64,
    pub modified_unix_ms: Option<u64>,
    pub sample_blake3: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceReference {
    pub path: PathBuf,
    pub fingerprint: SourceFingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ViewState {
    pub zoom: f32,
    pub center_x: f32,
    pub center_y: f32,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            zoom: 0.0,
            center_x: 0.5,
            center_y: 0.5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CropRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl CropRect {
    pub const FULL: Self = Self {
        x: 0.0,
        y: 0.0,
        width: 1.0,
        height: 1.0,
    };

    #[must_use]
    pub fn rotated_right(self) -> Self {
        Self {
            x: 1.0 - self.y - self.height,
            y: self.x,
            width: self.height,
            height: self.width,
        }
        .normalized()
    }

    #[must_use]
    pub fn rotated_left(self) -> Self {
        Self {
            x: self.y,
            y: 1.0 - self.x - self.width,
            width: self.height,
            height: self.width,
        }
        .normalized()
    }

    #[must_use]
    pub fn normalized(self) -> Self {
        let width = self.width.clamp(MIN_CROP_EXTENT, 1.0);
        let height = self.height.clamp(MIN_CROP_EXTENT, 1.0);
        Self {
            x: self.x.clamp(0.0, 1.0 - width),
            y: self.y.clamp(0.0, 1.0 - height),
            width,
            height,
        }
    }

    #[must_use]
    pub fn is_full(self) -> bool {
        (self.x).abs() < f32::EPSILON
            && (self.y).abs() < f32::EPSILON
            && (self.width - 1.0).abs() < f32::EPSILON
            && (self.height - 1.0).abs() < f32::EPSILON
    }
}

impl Default for CropRect {
    fn default() -> Self {
        Self::FULL
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl FrameColor {
    pub const WHITE: Self = Self {
        r: 255,
        g: 255,
        b: 255,
    };
    pub const BLACK: Self = Self { r: 0, g: 0, b: 0 };
}

impl Default for FrameColor {
    fn default() -> Self {
        Self::WHITE
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WhiteBalance {
    pub temperature: f32,
    pub tint: f32,
}

impl WhiteBalance {
    pub const IDENTITY: Self = Self {
        temperature: 0.0,
        tint: 0.0,
    };

    #[must_use]
    pub fn is_identity(self) -> bool {
        self.temperature.abs() < f32::EPSILON && self.tint.abs() < f32::EPSILON
    }
}

impl Default for WhiteBalance {
    fn default() -> Self {
        Self::IDENTITY
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ToneCurve {
    #[serde(default = "default_shadow_input")]
    pub shadow_input: f32,
    pub shadows: f32,
    #[serde(default = "default_midtone_input")]
    pub midtone_input: f32,
    #[serde(default = "default_midtones")]
    pub midtones: f32,
    #[serde(default = "default_highlight_input")]
    pub highlight_input: f32,
    pub highlights: f32,
}

const fn default_shadow_input() -> f32 {
    0.25
}

const fn default_midtone_input() -> f32 {
    0.5
}

const fn default_midtones() -> f32 {
    0.5
}

const fn default_highlight_input() -> f32 {
    0.75
}

impl ToneCurve {
    pub const IDENTITY: Self = Self {
        shadow_input: 0.25,
        shadows: 0.25,
        midtone_input: 0.5,
        midtones: 0.5,
        highlight_input: 0.75,
        highlights: 0.75,
    };

    #[must_use]
    pub fn is_identity(self) -> bool {
        (self.shadow_input - self.shadows).abs() < f32::EPSILON
            && (self.midtone_input - self.midtones).abs() < f32::EPSILON
            && (self.highlight_input - self.highlights).abs() < f32::EPSILON
    }

    /// Samples the shape-preserving cubic Hermite curve through the five
    /// control points. Values outside the display-referred 0..=1 interval are
    /// preserved so extended-range linear scRGB data is not destroyed.
    #[must_use]
    pub fn sample(self, x: f32) -> f32 {
        if !(0.0..=1.0).contains(&x) {
            return x;
        }
        let inputs = [
            0.0,
            self.shadow_input,
            self.midtone_input,
            self.highlight_input,
            1.0,
        ];
        let values = [0.0, self.shadows, self.midtones, self.highlights, 1.0];
        let tangents = shape_preserving_tangents(inputs, values);
        let segment = (0..4)
            .find(|&segment| x <= inputs[segment + 1])
            .unwrap_or(3);
        let interval = inputs[segment + 1] - inputs[segment];
        let local_x = (x - inputs[segment]) / interval;
        let local_x2 = local_x * local_x;
        let local_x3 = local_x2 * local_x;
        let h00 = 2.0 * local_x3 - 3.0 * local_x2 + 1.0;
        let h10 = local_x3 - 2.0 * local_x2 + local_x;
        let h01 = -2.0 * local_x3 + 3.0 * local_x2;
        let h11 = local_x3 - local_x2;
        h00 * values[segment]
            + h10 * interval * tangents[segment]
            + h01 * values[segment + 1]
            + h11 * interval * tangents[segment + 1]
    }
}

impl Default for ToneCurve {
    fn default() -> Self {
        Self::IDENTITY
    }
}

fn shape_preserving_tangents(inputs: [f32; 5], values: [f32; 5]) -> [f32; 5] {
    let intervals = [
        inputs[1] - inputs[0],
        inputs[2] - inputs[1],
        inputs[3] - inputs[2],
        inputs[4] - inputs[3],
    ];
    let slopes = [
        (values[1] - values[0]) / intervals[0],
        (values[2] - values[1]) / intervals[1],
        (values[3] - values[2]) / intervals[2],
        (values[4] - values[3]) / intervals[3],
    ];
    let mut tangents = [slopes[0], 0.0, 0.0, 0.0, slopes[3]];
    for point in 1..4 {
        let before = slopes[point - 1];
        let after = slopes[point];
        tangents[point] = if before * after <= 0.0 {
            0.0
        } else {
            let before_interval = intervals[point - 1];
            let after_interval = intervals[point];
            let before_weight = 2.0 * after_interval + before_interval;
            let after_weight = after_interval + 2.0 * before_interval;
            (before_weight + after_weight) / (before_weight / before + after_weight / after)
        };
    }
    for segment in 0..4 {
        if slopes[segment].abs() < f32::EPSILON {
            tangents[segment] = 0.0;
            tangents[segment + 1] = 0.0;
            continue;
        }
        let a = tangents[segment] / slopes[segment];
        let b = tangents[segment + 1] / slopes[segment];
        let magnitude = a * a + b * b;
        if magnitude > 9.0 {
            let scale = 3.0 / magnitude.sqrt();
            tangents[segment] = scale * a * slopes[segment];
            tangents[segment + 1] = scale * b * slopes[segment];
        }
    }
    tangents
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Operation {
    Rotate { quarter_turns: u8 },
    Crop { rect: CropRect },
    WhiteBalance { adjustment: WhiteBalance },
    Exposure { ev: f32 },
    Contrast { amount: f32 },
    ToneCurve { curve: ToneCurve },
    Saturation { amount: f32 },
    Sharpness { amount: f32 },
    Frame { width_pct: f32, color: FrameColor },
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    SetExposure {
        before: f32,
        after: f32,
    },
    SetContrast {
        before: f32,
        after: f32,
    },
    SetWhiteBalance {
        before: WhiteBalance,
        after: WhiteBalance,
    },
    SetSaturation {
        before: f32,
        after: f32,
    },
    SetSharpness {
        before: f32,
        after: f32,
    },
    SetCrop {
        before: CropRect,
        after: CropRect,
    },
    Rotate {
        before: u8,
        after: u8,
        crop_before: CropRect,
        crop_after: CropRect,
    },
    SetToneCurve {
        before: ToneCurve,
        after: ToneCurve,
    },
    SetFrame {
        before_width_pct: f32,
        before_color: FrameColor,
        after_width_pct: f32,
        after_color: FrameColor,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CommandHistory {
    #[serde(default)]
    undo: Vec<Command>,
    #[serde(default)]
    redo: Vec<Command>,
}

impl CommandHistory {
    #[must_use]
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    #[must_use]
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    #[must_use]
    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    #[must_use]
    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }

    fn push(&mut self, command: Command) {
        self.undo.push(command);
        self.redo.clear();
        if self.undo.len() > MAX_HISTORY_LEN {
            let overflow = self.undo.len() - MAX_HISTORY_LEN;
            self.undo.drain(0..overflow);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectDocument {
    pub schema_version: u32,
    pub source: SourceReference,
    #[serde(default = "default_operations")]
    pub operations: Vec<Operation>,
    #[serde(default)]
    pub view: ViewState,
    #[serde(default)]
    pub history: CommandHistory,
}

fn default_operations() -> Vec<Operation> {
    vec![
        Operation::Rotate { quarter_turns: 0 },
        Operation::Crop {
            rect: CropRect::FULL,
        },
        Operation::WhiteBalance {
            adjustment: WhiteBalance::IDENTITY,
        },
        Operation::Exposure { ev: 0.0 },
        Operation::Contrast { amount: 0.0 },
        Operation::ToneCurve {
            curve: ToneCurve::IDENTITY,
        },
        Operation::Saturation { amount: 0.0 },
        Operation::Sharpness { amount: 0.0 },
        Operation::Frame {
            width_pct: 0.0,
            color: FrameColor::WHITE,
        },
    ]
}

#[derive(Debug, Error, PartialEq)]
pub enum DocumentError {
    #[error("unsupported project schema version {found}; newest supported version is {supported}")]
    UnsupportedSchema { found: u32, supported: u32 },
    #[error("exposure must be finite and between -5 and +5 EV")]
    InvalidExposure,
    #[error("contrast must be finite and between -100 and +100")]
    InvalidContrast,
    #[error("temperature and tint must be finite and between -100 and +100")]
    InvalidWhiteBalance,
    #[error("saturation must be finite and between -100 and +100")]
    InvalidSaturation,
    #[error("sharpness must be finite and between 0 and 300")]
    InvalidSharpness,
    #[error("rotation must be between 0 and 3 quarter turns")]
    InvalidRotation,
    #[error("crop rectangle must be finite, inside the image, and at least 1% wide and high")]
    InvalidCrop,
    #[error("tone curve points must be finite, inside the 0 to 1 interval, and ordered by input")]
    InvalidToneCurve,
    #[error("frame width must be finite and between 0 and 50 percent")]
    InvalidFrame,
}

impl ProjectDocument {
    #[must_use]
    pub fn new(source: SourceReference) -> Self {
        Self {
            schema_version: PROJECT_SCHEMA_VERSION,
            source,
            operations: default_operations(),
            view: ViewState::default(),
            history: CommandHistory::default(),
        }
    }

    pub fn validate(&self) -> Result<(), DocumentError> {
        if self.schema_version > PROJECT_SCHEMA_VERSION {
            return Err(DocumentError::UnsupportedSchema {
                found: self.schema_version,
                supported: PROJECT_SCHEMA_VERSION,
            });
        }
        for operation in &self.operations {
            match *operation {
                Operation::Rotate { quarter_turns } => validate_rotation(quarter_turns)?,
                Operation::Crop { rect } => validate_crop(rect)?,
                Operation::WhiteBalance { adjustment } => validate_white_balance(adjustment)?,
                Operation::Exposure { ev } => validate_exposure(ev)?,
                Operation::Contrast { amount } => validate_contrast(amount)?,
                Operation::ToneCurve { curve } => validate_tone_curve(curve)?,
                Operation::Saturation { amount } => validate_saturation(amount)?,
                Operation::Sharpness { amount } => validate_sharpness(amount)?,
                Operation::Frame { width_pct, .. } => validate_frame(width_pct)?,
            }
        }
        Ok(())
    }

    pub fn upgrade_to_latest(&mut self) -> Result<(), DocumentError> {
        self.validate()?;
        if self.schema_version < 6
            && !self
                .operations
                .iter()
                .any(|operation| matches!(operation, Operation::WhiteBalance { .. }))
        {
            let index = self
                .operations
                .iter()
                .position(|operation| matches!(operation, Operation::Exposure { .. }))
                .unwrap_or(self.operations.len());
            self.operations.insert(
                index,
                Operation::WhiteBalance {
                    adjustment: WhiteBalance::IDENTITY,
                },
            );
        }
        if self.schema_version < 7
            && !self
                .operations
                .iter()
                .any(|operation| matches!(operation, Operation::Saturation { .. }))
        {
            let index = self
                .operations
                .iter()
                .rposition(|operation| matches!(operation, Operation::ToneCurve { .. }))
                .map_or(self.operations.len(), |index| index + 1);
            self.operations
                .insert(index, Operation::Saturation { amount: 0.0 });
        }
        if self.schema_version < 8
            && !self
                .operations
                .iter()
                .any(|operation| matches!(operation, Operation::Sharpness { .. }))
        {
            let index = self
                .operations
                .iter()
                .rposition(|operation| matches!(operation, Operation::Saturation { .. }))
                .map_or(self.operations.len(), |index| index + 1);
            self.operations
                .insert(index, Operation::Sharpness { amount: 0.0 });
        }
        if self.schema_version < 9
            && !self
                .operations
                .iter()
                .any(|operation| matches!(operation, Operation::Contrast { .. }))
        {
            let index = self
                .operations
                .iter()
                .rposition(|operation| matches!(operation, Operation::Exposure { .. }))
                .map_or(self.operations.len(), |index| index + 1);
            self.operations
                .insert(index, Operation::Contrast { amount: 0.0 });
        }
        if self.schema_version < 10
            && !self
                .operations
                .iter()
                .any(|operation| matches!(operation, Operation::Frame { .. }))
        {
            self.operations.push(Operation::Frame {
                width_pct: 0.0,
                color: FrameColor::WHITE,
            });
        }
        self.schema_version = PROJECT_SCHEMA_VERSION;
        Ok(())
    }

    #[must_use]
    pub fn exposure_ev(&self) -> f32 {
        self.operations
            .iter()
            .rev()
            .find_map(|operation| match *operation {
                Operation::Exposure { ev } => Some(ev),
                _ => None,
            })
            .unwrap_or(0.0)
    }

    #[must_use]
    pub fn contrast(&self) -> f32 {
        self.operations
            .iter()
            .rev()
            .find_map(|operation| match *operation {
                Operation::Contrast { amount } => Some(amount),
                _ => None,
            })
            .unwrap_or(0.0)
    }

    #[must_use]
    pub fn white_balance(&self) -> WhiteBalance {
        self.operations
            .iter()
            .rev()
            .find_map(|operation| match *operation {
                Operation::WhiteBalance { adjustment } => Some(adjustment),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[must_use]
    pub fn rotation_quarter_turns(&self) -> u8 {
        self.operations
            .iter()
            .rev()
            .find_map(|operation| match *operation {
                Operation::Rotate { quarter_turns } => Some(quarter_turns),
                _ => None,
            })
            .unwrap_or(0)
    }

    #[must_use]
    pub fn crop_rect(&self) -> CropRect {
        self.operations
            .iter()
            .rev()
            .find_map(|operation| match *operation {
                Operation::Crop { rect } => Some(rect),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[must_use]
    pub fn tone_curve(&self) -> ToneCurve {
        self.operations
            .iter()
            .rev()
            .find_map(|operation| match *operation {
                Operation::ToneCurve { curve } => Some(curve),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[must_use]
    pub fn saturation(&self) -> f32 {
        self.operations
            .iter()
            .rev()
            .find_map(|operation| match *operation {
                Operation::Saturation { amount } => Some(amount),
                _ => None,
            })
            .unwrap_or(0.0)
    }

    #[must_use]
    pub fn sharpness(&self) -> f32 {
        self.operations
            .iter()
            .rev()
            .find_map(|operation| match *operation {
                Operation::Sharpness { amount } => Some(amount),
                _ => None,
            })
            .unwrap_or(0.0)
    }

    #[must_use]
    pub fn frame(&self) -> (f32, FrameColor) {
        self.operations
            .iter()
            .rev()
            .find_map(|operation| match *operation {
                Operation::Frame { width_pct, color } => Some((width_pct, color)),
                _ => None,
            })
            .unwrap_or((0.0, FrameColor::WHITE))
    }

    #[must_use]
    pub fn output_dimensions(&self, source_width: u32, source_height: u32) -> (u32, u32) {
        let (rotated_width, rotated_height) = if self.rotation_quarter_turns().is_multiple_of(2) {
            (source_width, source_height)
        } else {
            (source_height, source_width)
        };
        let crop = self.crop_rect();
        let cropped_w = ((rotated_width as f32 * crop.width).round() as u32).max(1);
        let cropped_h = ((rotated_height as f32 * crop.height).round() as u32).max(1);
        let (frame_width_pct, _) = self.frame();
        if frame_width_pct > f32::EPSILON {
            let border_px =
                ((cropped_w.min(cropped_h) as f32 * frame_width_pct / 100.0).round() as u32).max(1);
            (cropped_w + 2 * border_px, cropped_h + 2 * border_px)
        } else {
            (cropped_w, cropped_h)
        }
    }

    /// Updates the visible state without creating a history entry.
    ///
    /// The UI uses this during continuous slider movement, then calls
    /// [`Self::commit_exposure`] once the edit transaction settles.
    pub fn preview_exposure(&mut self, ev: f32) -> Result<(), DocumentError> {
        validate_exposure(ev)?;
        if let Some(Operation::Exposure { ev: current }) = self
            .operations
            .iter_mut()
            .rev()
            .find(|operation| matches!(operation, Operation::Exposure { .. }))
        {
            *current = ev;
        } else {
            self.operations.push(Operation::Exposure { ev });
        }
        Ok(())
    }

    pub fn commit_exposure(&mut self, before: f32, after: f32) -> Result<(), DocumentError> {
        validate_exposure(before)?;
        self.preview_exposure(after)?;
        if (before - after).abs() > f32::EPSILON {
            self.history.push(Command::SetExposure { before, after });
        }
        Ok(())
    }

    pub fn preview_contrast(&mut self, amount: f32) -> Result<(), DocumentError> {
        validate_contrast(amount)?;
        if let Some(Operation::Contrast { amount: current }) = self
            .operations
            .iter_mut()
            .rev()
            .find(|operation| matches!(operation, Operation::Contrast { .. }))
        {
            *current = amount;
        } else {
            self.operations.push(Operation::Contrast { amount });
        }
        Ok(())
    }

    pub fn commit_contrast(&mut self, before: f32, after: f32) -> Result<(), DocumentError> {
        validate_contrast(before)?;
        self.preview_contrast(after)?;
        if (before - after).abs() > f32::EPSILON {
            self.history.push(Command::SetContrast { before, after });
        }
        Ok(())
    }

    pub fn preview_white_balance(&mut self, adjustment: WhiteBalance) -> Result<(), DocumentError> {
        validate_white_balance(adjustment)?;
        if let Some(Operation::WhiteBalance {
            adjustment: current,
        }) = self
            .operations
            .iter_mut()
            .rev()
            .find(|operation| matches!(operation, Operation::WhiteBalance { .. }))
        {
            *current = adjustment;
        } else {
            self.operations.push(Operation::WhiteBalance { adjustment });
        }
        Ok(())
    }

    pub fn commit_white_balance(
        &mut self,
        before: WhiteBalance,
        after: WhiteBalance,
    ) -> Result<(), DocumentError> {
        validate_white_balance(before)?;
        self.preview_white_balance(after)?;
        if before != after {
            self.history
                .push(Command::SetWhiteBalance { before, after });
        }
        Ok(())
    }

    pub fn preview_saturation(&mut self, amount: f32) -> Result<(), DocumentError> {
        validate_saturation(amount)?;
        if let Some(Operation::Saturation { amount: current }) = self
            .operations
            .iter_mut()
            .rev()
            .find(|operation| matches!(operation, Operation::Saturation { .. }))
        {
            *current = amount;
        } else {
            self.operations.push(Operation::Saturation { amount });
        }
        Ok(())
    }

    pub fn commit_saturation(&mut self, before: f32, after: f32) -> Result<(), DocumentError> {
        validate_saturation(before)?;
        self.preview_saturation(after)?;
        if (before - after).abs() > f32::EPSILON {
            self.history.push(Command::SetSaturation { before, after });
        }
        Ok(())
    }

    pub fn preview_sharpness(&mut self, amount: f32) -> Result<(), DocumentError> {
        validate_sharpness(amount)?;
        if let Some(Operation::Sharpness { amount: current }) = self
            .operations
            .iter_mut()
            .rev()
            .find(|operation| matches!(operation, Operation::Sharpness { .. }))
        {
            *current = amount;
        } else {
            self.operations.push(Operation::Sharpness { amount });
        }
        Ok(())
    }

    pub fn commit_sharpness(&mut self, before: f32, after: f32) -> Result<(), DocumentError> {
        validate_sharpness(before)?;
        self.preview_sharpness(after)?;
        if (before - after).abs() > f32::EPSILON {
            self.history.push(Command::SetSharpness { before, after });
        }
        Ok(())
    }

    pub fn preview_frame(
        &mut self,
        width_pct: f32,
        color: FrameColor,
    ) -> Result<(), DocumentError> {
        validate_frame(width_pct)?;
        if let Some(Operation::Frame {
            width_pct: current_w,
            color: current_c,
        }) = self
            .operations
            .iter_mut()
            .rev()
            .find(|op| matches!(op, Operation::Frame { .. }))
        {
            *current_w = width_pct;
            *current_c = color;
        } else {
            self.operations.push(Operation::Frame { width_pct, color });
        }
        Ok(())
    }

    pub fn commit_frame(
        &mut self,
        before_width_pct: f32,
        before_color: FrameColor,
        after_width_pct: f32,
        after_color: FrameColor,
    ) -> Result<(), DocumentError> {
        validate_frame(before_width_pct)?;
        self.preview_frame(after_width_pct, after_color)?;
        if (before_width_pct - after_width_pct).abs() > f32::EPSILON || before_color != after_color
        {
            self.history.push(Command::SetFrame {
                before_width_pct,
                before_color,
                after_width_pct,
                after_color,
            });
        }
        Ok(())
    }

    pub fn preview_crop(&mut self, rect: CropRect) -> Result<(), DocumentError> {
        validate_crop(rect)?;
        if let Some(Operation::Crop { rect: current }) = self
            .operations
            .iter_mut()
            .rev()
            .find(|operation| matches!(operation, Operation::Crop { .. }))
        {
            *current = rect;
        } else {
            self.operations.push(Operation::Crop { rect });
        }
        Ok(())
    }

    pub fn commit_crop(&mut self, before: CropRect, after: CropRect) -> Result<(), DocumentError> {
        validate_crop(before)?;
        self.preview_crop(after)?;
        if before != after {
            self.history.push(Command::SetCrop { before, after });
        }
        Ok(())
    }

    pub fn rotate_by(&mut self, quarter_turn_delta: i8) -> Result<(), DocumentError> {
        let before = self.rotation_quarter_turns();
        let crop_before = self.crop_rect();
        let normalized_delta = quarter_turn_delta.rem_euclid(4) as u8;
        let after = (before + normalized_delta) % 4;
        let mut crop_after = crop_before;
        for _ in 0..normalized_delta {
            crop_after = crop_after.rotated_right();
        }
        self.preview_rotation(after)?;
        self.preview_crop(crop_after)?;
        if before != after {
            self.history.push(Command::Rotate {
                before,
                after,
                crop_before,
                crop_after,
            });
        }
        Ok(())
    }

    pub fn preview_tone_curve(&mut self, curve: ToneCurve) -> Result<(), DocumentError> {
        validate_tone_curve(curve)?;
        if let Some(Operation::ToneCurve { curve: current }) = self
            .operations
            .iter_mut()
            .rev()
            .find(|operation| matches!(operation, Operation::ToneCurve { .. }))
        {
            *current = curve;
        } else {
            self.operations.push(Operation::ToneCurve { curve });
        }
        Ok(())
    }

    pub fn commit_tone_curve(
        &mut self,
        before: ToneCurve,
        after: ToneCurve,
    ) -> Result<(), DocumentError> {
        validate_tone_curve(before)?;
        self.preview_tone_curve(after)?;
        if before != after {
            self.history.push(Command::SetToneCurve { before, after });
        }
        Ok(())
    }

    fn preview_rotation(&mut self, quarter_turns: u8) -> Result<(), DocumentError> {
        validate_rotation(quarter_turns)?;
        if let Some(Operation::Rotate {
            quarter_turns: current,
        }) = self
            .operations
            .iter_mut()
            .rev()
            .find(|operation| matches!(operation, Operation::Rotate { .. }))
        {
            *current = quarter_turns;
        } else {
            self.operations.push(Operation::Rotate { quarter_turns });
        }
        Ok(())
    }

    pub fn undo(&mut self) -> bool {
        let Some(command) = self.history.undo.pop() else {
            return false;
        };
        match command {
            Command::SetExposure { before, .. } => {
                let _ = self.preview_exposure(before);
            }
            Command::SetContrast { before, .. } => {
                let _ = self.preview_contrast(before);
            }
            Command::SetWhiteBalance { before, .. } => {
                let _ = self.preview_white_balance(before);
            }
            Command::SetSaturation { before, .. } => {
                let _ = self.preview_saturation(before);
            }
            Command::SetSharpness { before, .. } => {
                let _ = self.preview_sharpness(before);
            }
            Command::SetCrop { before, .. } => {
                let _ = self.preview_crop(before);
            }
            Command::Rotate {
                before,
                crop_before,
                ..
            } => {
                let _ = self.preview_rotation(before);
                let _ = self.preview_crop(crop_before);
            }
            Command::SetToneCurve { before, .. } => {
                let _ = self.preview_tone_curve(before);
            }
            Command::SetFrame {
                before_width_pct,
                before_color,
                ..
            } => {
                let _ = self.preview_frame(before_width_pct, before_color);
            }
        }
        self.history.redo.push(command);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(command) = self.history.redo.pop() else {
            return false;
        };
        match command {
            Command::SetExposure { after, .. } => {
                let _ = self.preview_exposure(after);
            }
            Command::SetContrast { after, .. } => {
                let _ = self.preview_contrast(after);
            }
            Command::SetWhiteBalance { after, .. } => {
                let _ = self.preview_white_balance(after);
            }
            Command::SetSaturation { after, .. } => {
                let _ = self.preview_saturation(after);
            }
            Command::SetSharpness { after, .. } => {
                let _ = self.preview_sharpness(after);
            }
            Command::SetCrop { after, .. } => {
                let _ = self.preview_crop(after);
            }
            Command::Rotate {
                after, crop_after, ..
            } => {
                let _ = self.preview_rotation(after);
                let _ = self.preview_crop(crop_after);
            }
            Command::SetToneCurve { after, .. } => {
                let _ = self.preview_tone_curve(after);
            }
            Command::SetFrame {
                after_width_pct,
                after_color,
                ..
            } => {
                let _ = self.preview_frame(after_width_pct, after_color);
            }
        }
        self.history.undo.push(command);
        true
    }
}

fn validate_rotation(quarter_turns: u8) -> Result<(), DocumentError> {
    if quarter_turns <= 3 {
        Ok(())
    } else {
        Err(DocumentError::InvalidRotation)
    }
}

fn validate_crop(rect: CropRect) -> Result<(), DocumentError> {
    let values = [rect.x, rect.y, rect.width, rect.height];
    let epsilon = 0.000_01;
    if values.into_iter().all(f32::is_finite)
        && rect.x >= 0.0
        && rect.y >= 0.0
        && rect.width >= MIN_CROP_EXTENT
        && rect.height >= MIN_CROP_EXTENT
        && rect.x + rect.width <= 1.0 + epsilon
        && rect.y + rect.height <= 1.0 + epsilon
    {
        Ok(())
    } else {
        Err(DocumentError::InvalidCrop)
    }
}

fn validate_tone_curve(curve: ToneCurve) -> Result<(), DocumentError> {
    const MIN_INPUT_GAP: f32 = 0.001;
    if curve.shadow_input.is_finite()
        && curve.shadows.is_finite()
        && curve.midtone_input.is_finite()
        && curve.midtones.is_finite()
        && curve.highlight_input.is_finite()
        && curve.highlights.is_finite()
        && curve.shadow_input >= MIN_INPUT_GAP
        && curve.midtone_input - curve.shadow_input >= MIN_INPUT_GAP
        && curve.highlight_input - curve.midtone_input >= MIN_INPUT_GAP
        && 1.0 - curve.highlight_input >= MIN_INPUT_GAP
        && (0.0..=1.0).contains(&curve.shadows)
        && (0.0..=1.0).contains(&curve.midtones)
        && (0.0..=1.0).contains(&curve.highlights)
    {
        Ok(())
    } else {
        Err(DocumentError::InvalidToneCurve)
    }
}

fn validate_white_balance(adjustment: WhiteBalance) -> Result<(), DocumentError> {
    if adjustment.temperature.is_finite()
        && adjustment.tint.is_finite()
        && (-100.0..=100.0).contains(&adjustment.temperature)
        && (-100.0..=100.0).contains(&adjustment.tint)
    {
        Ok(())
    } else {
        Err(DocumentError::InvalidWhiteBalance)
    }
}

fn validate_saturation(amount: f32) -> Result<(), DocumentError> {
    if amount.is_finite() && (-100.0..=100.0).contains(&amount) {
        Ok(())
    } else {
        Err(DocumentError::InvalidSaturation)
    }
}

fn validate_sharpness(amount: f32) -> Result<(), DocumentError> {
    if amount.is_finite() && (0.0..=300.0).contains(&amount) {
        Ok(())
    } else {
        Err(DocumentError::InvalidSharpness)
    }
}

fn validate_exposure(ev: f32) -> Result<(), DocumentError> {
    if ev.is_finite() && (-5.0..=5.0).contains(&ev) {
        Ok(())
    } else {
        Err(DocumentError::InvalidExposure)
    }
}

fn validate_contrast(amount: f32) -> Result<(), DocumentError> {
    if amount.is_finite() && (-100.0..=100.0).contains(&amount) {
        Ok(())
    } else {
        Err(DocumentError::InvalidContrast)
    }
}

fn validate_frame(width_pct: f32) -> Result<(), DocumentError> {
    if width_pct.is_finite() && (0.0..=MAX_FRAME_WIDTH_PCT).contains(&width_pct) {
        Ok(())
    } else {
        Err(DocumentError::InvalidFrame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> ProjectDocument {
        ProjectDocument::new(SourceReference {
            path: PathBuf::from("/photos/example.jpg"),
            fingerprint: SourceFingerprint {
                byte_len: 42,
                modified_unix_ms: Some(10),
                sample_blake3: "abc".into(),
                width: 100,
                height: 50,
            },
        })
    }

    #[test]
    fn exposure_transaction_is_undoable_and_redoable() {
        let mut document = document();
        document.preview_exposure(0.8).unwrap();
        document.commit_exposure(0.0, 0.8).unwrap();

        assert_eq!(document.exposure_ev(), 0.8);
        assert!(document.undo());
        assert_eq!(document.exposure_ev(), 0.0);
        assert!(document.redo());
        assert_eq!(document.exposure_ev(), 0.8);
    }

    #[test]
    fn contrast_transaction_is_undoable_and_redoable() {
        let mut document = document();
        document.preview_contrast(35.0).unwrap();
        document.commit_contrast(0.0, 35.0).unwrap();

        assert_eq!(document.contrast(), 35.0);
        assert!(document.undo());
        assert_eq!(document.contrast(), 0.0);
        assert!(document.redo());
        assert_eq!(document.contrast(), 35.0);
    }

    #[test]
    fn white_balance_transaction_is_undoable_and_redoable() {
        let mut document = document();
        let adjusted = WhiteBalance {
            temperature: 42.0,
            tint: -17.0,
        };
        document.preview_white_balance(adjusted).unwrap();
        document
            .commit_white_balance(WhiteBalance::IDENTITY, adjusted)
            .unwrap();

        assert_eq!(document.white_balance(), adjusted);
        assert!(document.undo());
        assert_eq!(document.white_balance(), WhiteBalance::IDENTITY);
        assert!(document.redo());
        assert_eq!(document.white_balance(), adjusted);
    }

    #[test]
    fn saturation_transaction_is_undoable_and_redoable() {
        let mut document = document();
        document.preview_saturation(35.0).unwrap();
        document.commit_saturation(0.0, 35.0).unwrap();

        assert_eq!(document.saturation(), 35.0);
        assert!(document.undo());
        assert_eq!(document.saturation(), 0.0);
        assert!(document.redo());
        assert_eq!(document.saturation(), 35.0);
    }

    #[test]
    fn sharpness_transaction_is_undoable_and_redoable() {
        let mut document = document();
        document.preview_sharpness(240.0).unwrap();
        document.commit_sharpness(0.0, 240.0).unwrap();

        assert_eq!(document.sharpness(), 240.0);
        assert!(document.undo());
        assert_eq!(document.sharpness(), 0.0);
        assert!(document.redo());
        assert_eq!(document.sharpness(), 240.0);
    }

    #[test]
    fn new_edit_clears_redo_history() {
        let mut document = document();
        document.commit_exposure(0.0, 1.0).unwrap();
        assert!(document.undo());
        document.commit_exposure(0.0, -1.0).unwrap();
        assert!(!document.history.can_redo());
    }

    #[test]
    fn history_is_bounded() {
        let mut document = document();
        for index in 0..(MAX_HISTORY_LEN + 20) {
            let before = document.exposure_ev();
            let after = ((index % 80) as f32 / 10.0) - 4.0;
            document.commit_exposure(before, after).unwrap();
        }
        assert_eq!(document.history.undo_len(), MAX_HISTORY_LEN);
    }

    #[test]
    fn rejects_invalid_exposure() {
        let mut document = document();
        assert_eq!(
            document.preview_exposure(f32::NAN),
            Err(DocumentError::InvalidExposure)
        );
        assert_eq!(
            document.preview_exposure(5.1),
            Err(DocumentError::InvalidExposure)
        );
    }

    #[test]
    fn rejects_invalid_contrast() {
        let mut document = document();
        assert_eq!(
            document.preview_contrast(f32::NAN),
            Err(DocumentError::InvalidContrast)
        );
        assert_eq!(
            document.preview_contrast(100.1),
            Err(DocumentError::InvalidContrast)
        );
    }

    #[test]
    fn rejects_invalid_white_balance() {
        let mut document = document();
        assert_eq!(
            document.preview_white_balance(WhiteBalance {
                temperature: f32::NAN,
                tint: 0.0,
            }),
            Err(DocumentError::InvalidWhiteBalance)
        );
        assert_eq!(
            document.preview_white_balance(WhiteBalance {
                temperature: 0.0,
                tint: 100.1,
            }),
            Err(DocumentError::InvalidWhiteBalance)
        );
    }

    #[test]
    fn rejects_invalid_saturation() {
        let mut document = document();
        assert_eq!(
            document.preview_saturation(f32::NAN),
            Err(DocumentError::InvalidSaturation)
        );
        assert_eq!(
            document.preview_saturation(-100.1),
            Err(DocumentError::InvalidSaturation)
        );
    }

    #[test]
    fn rejects_invalid_sharpness() {
        let mut document = document();
        assert_eq!(
            document.preview_sharpness(f32::NAN),
            Err(DocumentError::InvalidSharpness)
        );
        assert_eq!(
            document.preview_sharpness(-0.1),
            Err(DocumentError::InvalidSharpness)
        );
        assert_eq!(
            document.preview_sharpness(300.1),
            Err(DocumentError::InvalidSharpness)
        );
    }

    #[test]
    fn crop_and_rotation_are_undoable() {
        let mut document = document();
        let crop = CropRect {
            x: 0.1,
            y: 0.2,
            width: 0.6,
            height: 0.5,
        };
        document.commit_crop(CropRect::FULL, crop).unwrap();
        document.rotate_by(1).unwrap();

        assert_eq!(document.rotation_quarter_turns(), 1);
        assert_eq!(document.crop_rect(), crop.rotated_right());
        assert_eq!(document.output_dimensions(100, 50), (25, 60));

        assert!(document.undo());
        assert_eq!(document.rotation_quarter_turns(), 0);
        assert_eq!(document.crop_rect(), crop);
        assert!(document.undo());
        assert_eq!(document.crop_rect(), CropRect::FULL);
        assert!(document.redo());
        assert_eq!(document.crop_rect(), crop);
    }

    #[test]
    fn rejects_invalid_crop_and_rotation() {
        let mut document = document();
        assert_eq!(
            document.preview_crop(CropRect {
                x: 0.9,
                y: 0.0,
                width: 0.2,
                height: 1.0,
            }),
            Err(DocumentError::InvalidCrop)
        );
        assert_eq!(
            document.preview_rotation(4),
            Err(DocumentError::InvalidRotation)
        );
    }

    #[test]
    fn tone_curve_is_shape_preserving_and_undoable() {
        let mut document = document();
        let curve = ToneCurve {
            shadow_input: 0.2,
            shadows: 0.12,
            midtone_input: 0.55,
            midtones: 0.42,
            highlight_input: 0.82,
            highlights: 0.88,
        };
        document
            .commit_tone_curve(ToneCurve::IDENTITY, curve)
            .unwrap();
        assert_eq!(document.tone_curve(), curve);
        assert_eq!(curve.sample(0.2), 0.12);
        assert_eq!(curve.sample(0.55), 0.42);
        assert_eq!(curve.sample(0.82), 0.88);
        let mut previous = 0.0;
        for index in 1..=1000 {
            let value = curve.sample(index as f32 / 1000.0);
            assert!(value >= previous);
            previous = value;
        }
        assert!(document.undo());
        assert_eq!(document.tone_curve(), ToneCurve::IDENTITY);
        assert!(document.redo());
        assert_eq!(document.tone_curve(), curve);
    }

    #[test]
    fn tone_curve_points_can_cross_without_segment_overshoot() {
        let curve = ToneCurve {
            shadow_input: 0.15,
            shadows: 0.9,
            midtone_input: 0.45,
            midtones: 0.1,
            highlight_input: 0.9,
            highlights: 0.8,
        };
        for index in 0..=1000 {
            let value = curve.sample(index as f32 / 1000.0);
            assert!((0.0..=1.0).contains(&value));
        }
    }

    #[test]
    fn rejects_invalid_tone_curve() {
        let mut document = document();
        assert_eq!(
            document.preview_tone_curve(ToneCurve {
                shadow_input: 0.25,
                shadows: 1.1,
                midtone_input: 0.5,
                midtones: 0.5,
                highlight_input: 0.75,
                highlights: 0.8,
            }),
            Err(DocumentError::InvalidToneCurve)
        );
        assert_eq!(
            document.preview_tone_curve(ToneCurve {
                shadow_input: 0.6,
                shadows: 0.2,
                midtone_input: 0.5,
                midtones: 0.5,
                highlight_input: 0.75,
                highlights: 0.8,
            }),
            Err(DocumentError::InvalidToneCurve)
        );
    }

    #[test]
    fn frame_transaction_is_undoable_and_redoable() {
        let mut document = document();
        let color = FrameColor {
            r: 30,
            g: 30,
            b: 30,
        };
        document.preview_frame(10.0, color).unwrap();
        document
            .commit_frame(0.0, FrameColor::WHITE, 10.0, color)
            .unwrap();
        assert_eq!(document.frame(), (10.0, color));
        assert!(document.undo());
        assert_eq!(document.frame(), (0.0, FrameColor::WHITE));
        assert!(document.redo());
        assert_eq!(document.frame(), (10.0, color));
    }

    #[test]
    fn rejects_invalid_frame() {
        let mut document = document();
        assert_eq!(
            document.preview_frame(f32::NAN, FrameColor::WHITE),
            Err(DocumentError::InvalidFrame)
        );
        assert_eq!(
            document.preview_frame(50.1, FrameColor::WHITE),
            Err(DocumentError::InvalidFrame)
        );
        assert_eq!(
            document.preview_frame(-0.1, FrameColor::WHITE),
            Err(DocumentError::InvalidFrame)
        );
    }

    #[test]
    fn schema_v9_migrates_with_neutral_frame() {
        let mut document = document();
        document.schema_version = 9;
        // Remove the Frame operation to simulate a v9 project.
        document
            .operations
            .retain(|op| !matches!(op, Operation::Frame { .. }));
        document.upgrade_to_latest().unwrap();
        assert_eq!(document.schema_version, PROJECT_SCHEMA_VERSION);
        let (width_pct, color) = document.frame();
        assert_eq!(width_pct, 0.0);
        assert_eq!(color, FrameColor::WHITE);
    }

    #[test]
    fn schema_v10_migrates_to_cat16_semantics_without_changing_parameters() {
        let mut document = document();
        let adjustment = WhiteBalance {
            temperature: 37.0,
            tint: -12.0,
        };
        document.schema_version = 10;
        document.preview_white_balance(adjustment).unwrap();

        document.upgrade_to_latest().unwrap();

        assert_eq!(document.schema_version, PROJECT_SCHEMA_VERSION);
        assert_eq!(document.white_balance(), adjustment);
    }
}
