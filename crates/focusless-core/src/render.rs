use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::Operation;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    pub output_width: u32,
    pub output_height: u32,
    pub zoom: f32,
    pub center_x: f32,
    pub center_y: f32,
}

impl Viewport {
    #[must_use]
    pub fn fit(output_width: u32, output_height: u32) -> Self {
        Self {
            output_width,
            output_height,
            zoom: 0.0,
            center_x: 0.5,
            center_y: 0.5,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreviewRequest {
    pub generation: u64,
    pub source_path: PathBuf,
    pub operations: Vec<Operation>,
    pub viewport: Viewport,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenderResult {
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub rgba8: Vec<u8>,
    pub effective_zoom: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Jpeg { quality: u8 },
    Png,
    WebP { quality: u8 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportRequest {
    pub source_path: PathBuf,
    pub destination_path: PathBuf,
    pub operations: Vec<Operation>,
    pub format: ExportFormat,
}

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("image dimensions are invalid")]
    InvalidDimensions,
    #[error("unsupported output format")]
    UnsupportedFormat,
    #[error("render was cancelled")]
    Cancelled,
    #[error("image engine error: {0}")]
    Engine(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
