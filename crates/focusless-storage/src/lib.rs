//! Durable project persistence and source fingerprinting.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use focusless_core::{
    DocumentError, PROJECT_SCHEMA_VERSION, ProjectDocument, SourceFingerprint, SourceReference,
};
use thiserror::Error;

const FINGERPRINT_CHUNK_LEN: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("project JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("project schema {found} is newer than supported schema {supported}")]
    UnsupportedSchema { found: u32, supported: u32 },
    #[error("project document is invalid: {0}")]
    InvalidDocument(#[from] DocumentError),
    #[error("project has no parent directory: {0}")]
    MissingParent(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceStatus {
    Current,
    Missing,
    Changed {
        expected: SourceFingerprint,
        actual: SourceFingerprint,
    },
}

pub fn fingerprint_source(
    path: &Path,
    width: u32,
    height: u32,
) -> Result<SourceFingerprint, StorageError> {
    let metadata = fs::metadata(path).map_err(|source| StorageError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut file = File::open(path).map_err(|source| StorageError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&metadata.len().to_le_bytes());

    let first_len = metadata.len().min(FINGERPRINT_CHUNK_LEN as u64) as usize;
    let mut buffer = vec![0_u8; first_len];
    file.read_exact(&mut buffer)
        .map_err(|source| StorageError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    hasher.update(&buffer);

    if metadata.len() > FINGERPRINT_CHUNK_LEN as u64 {
        let last_len = metadata.len().min(FINGERPRINT_CHUNK_LEN as u64) as usize;
        file.seek(SeekFrom::End(-(last_len as i64)))
            .map_err(|source| StorageError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        buffer.resize(last_len, 0);
        file.read_exact(&mut buffer)
            .map_err(|source| StorageError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        hasher.update(&buffer);
    }

    let modified_unix_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok());

    Ok(SourceFingerprint {
        byte_len: metadata.len(),
        modified_unix_ms,
        sample_blake3: hasher.finalize().to_hex().to_string(),
        width,
        height,
    })
}

pub fn save_project(path: &Path, document: &ProjectDocument) -> Result<(), StorageError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| StorageError::MissingParent(path.to_path_buf()))?;
    fs::create_dir_all(parent).map_err(|source| StorageError::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    let mut persisted = document.clone();
    persisted.upgrade_to_latest()?;
    if persisted.source.path.is_absolute()
        && let Some(relative) = pathdiff::diff_paths(&persisted.source.path, parent)
    {
        persisted.source.path = relative;
    }

    let payload = serde_json::to_vec_pretty(&persisted)?;
    let temporary_path = temporary_path_for(path);
    let mut temporary = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary_path)
        .map_err(|source| StorageError::Io {
            path: temporary_path.clone(),
            source,
        })?;
    temporary
        .write_all(&payload)
        .and_then(|()| temporary.write_all(b"\n"))
        .and_then(|()| temporary.sync_all())
        .map_err(|source| StorageError::Io {
            path: temporary_path.clone(),
            source,
        })?;
    drop(temporary);

    replace_file(&temporary_path, path).map_err(|source| StorageError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    sync_directory(parent)?;
    Ok(())
}

pub fn load_project(path: &Path) -> Result<ProjectDocument, StorageError> {
    let payload = fs::read(path).map_err(|source| StorageError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut document: ProjectDocument = serde_json::from_slice(&payload)?;
    if document.schema_version > PROJECT_SCHEMA_VERSION {
        return Err(StorageError::UnsupportedSchema {
            found: document.schema_version,
            supported: PROJECT_SCHEMA_VERSION,
        });
    }
    document.upgrade_to_latest()?;
    if document.source.path.is_relative() {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| StorageError::MissingParent(path.to_path_buf()))?;
        document.source.path = parent.join(&document.source.path);
    }
    Ok(document)
}

#[must_use]
pub fn recovery_candidate(path: &Path) -> Option<PathBuf> {
    let temporary = temporary_path_for(path);
    temporary.is_file().then_some(temporary)
}

pub fn inspect_source(
    source: &SourceReference,
    detected_width: u32,
    detected_height: u32,
) -> Result<SourceStatus, StorageError> {
    if !source.path.is_file() {
        return Ok(SourceStatus::Missing);
    }
    let actual = fingerprint_source(&source.path, detected_width, detected_height)?;
    if actual == source.fingerprint {
        Ok(SourceStatus::Current)
    } else {
        Ok(SourceStatus::Changed {
            expected: source.fingerprint.clone(),
            actual,
        })
    }
}

fn temporary_path_for(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
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

#[cfg(not(target_os = "windows"))]
fn sync_directory(path: &Path) -> Result<(), StorageError> {
    let directory = File::open(path).map_err(|source| StorageError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    directory.sync_all().map_err(|source| StorageError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(target_os = "windows")]
fn sync_directory(_path: &Path) -> Result<(), StorageError> {
    // MOVEFILE_WRITE_THROUGH flushes the replacement before returning. Opening
    // directories for sync requires Windows-specific backup semantics.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use focusless_core::{Operation, WhiteBalance};
    use tempfile::tempdir;

    fn source(path: PathBuf) -> SourceReference {
        SourceReference {
            path,
            fingerprint: SourceFingerprint {
                byte_len: 3,
                modified_unix_ms: None,
                sample_blake3: "hash".into(),
                width: 12,
                height: 8,
            },
        }
    }

    #[test]
    fn round_trip_uses_relative_source_path_and_restores_absolute_path() {
        let directory = tempdir().unwrap();
        let image_path = directory.path().join("image.jpg");
        let project_path = directory.path().join("edit.focusless");
        let mut document = ProjectDocument::new(source(image_path.clone()));

        save_project(&project_path, &document).unwrap();
        let raw = fs::read_to_string(&project_path).unwrap();
        assert!(raw.contains("\"path\": \"image.jpg\""));

        document.view.zoom = 2.0;
        save_project(&project_path, &document).unwrap();

        let restored = load_project(&project_path).unwrap();
        assert_eq!(restored.source.path, image_path);
        assert_eq!(restored.view.zoom, 2.0);
        assert_eq!(
            restored.operations,
            vec![
                Operation::Rotate { quarter_turns: 0 },
                Operation::Straighten { degrees: 0.0 },
                Operation::Crop {
                    rect: focusless_core::CropRect::FULL,
                },
                Operation::WhiteBalance {
                    adjustment: focusless_core::WhiteBalance::IDENTITY,
                },
                Operation::Exposure { ev: 0.0 },
                Operation::Contrast { amount: 0.0 },
                Operation::ShadowsHighlights {
                    adjustment: focusless_core::ShadowsHighlights::IDENTITY,
                },
                Operation::ToneCurve {
                    curve: focusless_core::ToneCurve::IDENTITY,
                },
                Operation::Saturation { amount: 0.0 },
                Operation::Matrix { enabled: false },
                Operation::Sharpness { amount: 0.0 },
                Operation::Frame {
                    width_pct: 0.0,
                    color: focusless_core::FrameColor::WHITE,
                },
            ]
        );
    }

    #[test]
    fn version_one_project_is_upgraded_when_loaded() {
        let directory = tempdir().unwrap();
        let project_path = directory.path().join("legacy.focusless");
        let image_path = directory.path().join("image.jpg");
        let mut document = ProjectDocument::new(source(image_path));
        document.schema_version = 1;
        document.operations = vec![Operation::Exposure { ev: 0.5 }];
        fs::write(&project_path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

        let restored = load_project(&project_path).unwrap();
        assert_eq!(restored.schema_version, PROJECT_SCHEMA_VERSION);
        assert_eq!(restored.exposure_ev(), 0.5);
        assert_eq!(restored.rotation_quarter_turns(), 0);
        assert!(restored.crop_rect().is_full());
    }

    #[test]
    fn version_two_project_gets_an_identity_tone_curve_when_loaded() {
        let directory = tempdir().unwrap();
        let project_path = directory.path().join("version-two.focusless");
        let image_path = directory.path().join("image.jpg");
        let mut document = ProjectDocument::new(source(image_path));
        document.schema_version = 2;
        document
            .operations
            .retain(|operation| !matches!(operation, Operation::ToneCurve { .. }));
        fs::write(&project_path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

        let restored = load_project(&project_path).unwrap();
        assert_eq!(restored.schema_version, PROJECT_SCHEMA_VERSION);
        assert_eq!(restored.tone_curve(), focusless_core::ToneCurve::IDENTITY);
    }

    #[test]
    fn version_three_tone_curve_gets_the_identity_midpoint() {
        let directory = tempdir().unwrap();
        let project_path = directory.path().join("version-three.focusless");
        let image_path = directory.path().join("image.jpg");
        let mut document = ProjectDocument::new(source(image_path));
        document.schema_version = 3;
        let mut json = serde_json::to_value(document).unwrap();
        let operations = json["operations"].as_array_mut().unwrap();
        let curve = operations
            .iter_mut()
            .find(|operation| operation["type"] == "tone_curve")
            .unwrap();
        let curve_fields = curve["curve"].as_object_mut().unwrap();
        curve_fields.remove("shadow_input");
        curve_fields.remove("midtone_input");
        curve_fields.remove("midtones");
        curve_fields.remove("highlight_input");
        fs::write(&project_path, serde_json::to_vec_pretty(&json).unwrap()).unwrap();

        let restored = load_project(&project_path).unwrap();
        assert_eq!(restored.schema_version, PROJECT_SCHEMA_VERSION);
        assert_eq!(restored.tone_curve().midtones, 0.5);
    }

    #[test]
    fn version_four_tone_curve_gets_default_input_positions() {
        let directory = tempdir().unwrap();
        let project_path = directory.path().join("version-four.focusless");
        let image_path = directory.path().join("image.jpg");
        let mut document = ProjectDocument::new(source(image_path));
        document.schema_version = 4;
        let mut json = serde_json::to_value(document).unwrap();
        let operations = json["operations"].as_array_mut().unwrap();
        let curve = operations
            .iter_mut()
            .find(|operation| operation["type"] == "tone_curve")
            .unwrap();
        let curve_fields = curve["curve"].as_object_mut().unwrap();
        curve_fields.remove("shadow_input");
        curve_fields.remove("midtone_input");
        curve_fields.remove("highlight_input");
        fs::write(&project_path, serde_json::to_vec_pretty(&json).unwrap()).unwrap();

        let restored = load_project(&project_path).unwrap();
        assert_eq!(restored.schema_version, PROJECT_SCHEMA_VERSION);
        assert_eq!(restored.tone_curve(), focusless_core::ToneCurve::IDENTITY);
    }

    #[test]
    fn version_five_project_gets_identity_white_balance_in_render_order() {
        let directory = tempdir().unwrap();
        let project_path = directory.path().join("version-five.focusless");
        let image_path = directory.path().join("image.jpg");
        let mut document = ProjectDocument::new(source(image_path));
        document.schema_version = 5;
        document
            .operations
            .retain(|operation| !matches!(operation, Operation::WhiteBalance { .. }));
        fs::write(&project_path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

        let restored = load_project(&project_path).unwrap();
        assert_eq!(restored.schema_version, PROJECT_SCHEMA_VERSION);
        assert_eq!(
            restored.white_balance(),
            focusless_core::WhiteBalance::IDENTITY
        );
        let white_balance_index = restored
            .operations
            .iter()
            .position(|operation| matches!(operation, Operation::WhiteBalance { .. }))
            .unwrap();
        let exposure_index = restored
            .operations
            .iter()
            .position(|operation| matches!(operation, Operation::Exposure { .. }))
            .unwrap();
        assert!(white_balance_index < exposure_index);
    }

    #[test]
    fn version_six_project_gets_neutral_saturation_after_tone_curve() {
        let directory = tempdir().unwrap();
        let project_path = directory.path().join("version-six.focusless");
        let image_path = directory.path().join("image.jpg");
        let mut document = ProjectDocument::new(source(image_path));
        document.schema_version = 6;
        document
            .operations
            .retain(|operation| !matches!(operation, Operation::Saturation { .. }));
        fs::write(&project_path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

        let restored = load_project(&project_path).unwrap();
        assert_eq!(restored.schema_version, PROJECT_SCHEMA_VERSION);
        assert_eq!(restored.saturation(), 0.0);
        let curve_index = restored
            .operations
            .iter()
            .position(|operation| matches!(operation, Operation::ToneCurve { .. }))
            .unwrap();
        let saturation_index = restored
            .operations
            .iter()
            .position(|operation| matches!(operation, Operation::Saturation { .. }))
            .unwrap();
        assert!(curve_index < saturation_index);
    }

    #[test]
    fn version_seven_project_gets_disabled_sharpness_after_saturation() {
        let directory = tempdir().unwrap();
        let project_path = directory.path().join("version-seven.focusless");
        let image_path = directory.path().join("image.jpg");
        let mut document = ProjectDocument::new(source(image_path));
        document.schema_version = 7;
        document
            .operations
            .retain(|operation| !matches!(operation, Operation::Sharpness { .. }));
        fs::write(&project_path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

        let restored = load_project(&project_path).unwrap();
        assert_eq!(restored.schema_version, PROJECT_SCHEMA_VERSION);
        assert_eq!(restored.sharpness(), 0.0);
        let saturation_index = restored
            .operations
            .iter()
            .position(|operation| matches!(operation, Operation::Saturation { .. }))
            .unwrap();
        let sharpness_index = restored
            .operations
            .iter()
            .position(|operation| matches!(operation, Operation::Sharpness { .. }))
            .unwrap();
        assert!(saturation_index < sharpness_index);
    }

    #[test]
    fn version_eight_project_gets_neutral_contrast_after_exposure() {
        let directory = tempdir().unwrap();
        let project_path = directory.path().join("version-eight.focusless");
        let image_path = directory.path().join("image.jpg");
        let mut document = ProjectDocument::new(source(image_path));
        document.schema_version = 8;
        document
            .operations
            .retain(|operation| !matches!(operation, Operation::Contrast { .. }));
        fs::write(&project_path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

        let restored = load_project(&project_path).unwrap();
        assert_eq!(restored.schema_version, PROJECT_SCHEMA_VERSION);
        assert_eq!(restored.contrast(), 0.0);
        let exposure_index = restored
            .operations
            .iter()
            .position(|operation| matches!(operation, Operation::Exposure { .. }))
            .unwrap();
        let contrast_index = restored
            .operations
            .iter()
            .position(|operation| matches!(operation, Operation::Contrast { .. }))
            .unwrap();
        assert!(exposure_index < contrast_index);
    }

    #[test]
    fn version_nine_project_gets_neutral_frame_at_end() {
        let directory = tempdir().unwrap();
        let project_path = directory.path().join("version-nine.focusless");
        let image_path = directory.path().join("image.jpg");
        let mut document = ProjectDocument::new(source(image_path));
        document.schema_version = 9;
        document
            .operations
            .retain(|operation| !matches!(operation, Operation::Frame { .. }));
        fs::write(&project_path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

        let restored = load_project(&project_path).unwrap();
        assert_eq!(restored.schema_version, PROJECT_SCHEMA_VERSION);
        let (frame_width_pct, frame_color) = restored.frame();
        assert_eq!(frame_width_pct, 0.0);
        assert_eq!(frame_color, focusless_core::FrameColor::WHITE);
        let sharpness_index = restored
            .operations
            .iter()
            .position(|operation| matches!(operation, Operation::Sharpness { .. }))
            .unwrap();
        let frame_index = restored
            .operations
            .iter()
            .position(|operation| matches!(operation, Operation::Frame { .. }))
            .unwrap();
        assert!(sharpness_index < frame_index);
    }

    #[test]
    fn version_ten_project_adopts_cat16_white_balance_semantics() {
        let directory = tempdir().unwrap();
        let project_path = directory.path().join("version-ten.focusless");
        let image_path = directory.path().join("image.jpg");
        let adjustment = WhiteBalance {
            temperature: 37.0,
            tint: -12.0,
        };
        let mut document = ProjectDocument::new(source(image_path));
        document.schema_version = 10;
        document.preview_white_balance(adjustment).unwrap();
        fs::write(&project_path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

        let restored = load_project(&project_path).unwrap();

        assert_eq!(restored.schema_version, PROJECT_SCHEMA_VERSION);
        assert_eq!(restored.white_balance(), adjustment);
    }

    #[test]
    fn version_eleven_project_gets_neutral_straighten_after_rotation() {
        let directory = tempdir().unwrap();
        let project_path = directory.path().join("version-eleven.focusless");
        let image_path = directory.path().join("image.jpg");
        let mut document = ProjectDocument::new(source(image_path));
        document.schema_version = 11;
        document
            .operations
            .retain(|operation| !matches!(operation, Operation::Straighten { .. }));
        fs::write(&project_path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

        let restored = load_project(&project_path).unwrap();

        assert_eq!(restored.schema_version, PROJECT_SCHEMA_VERSION);
        assert_eq!(restored.straighten_degrees(), 0.0);
        let rotate_index = restored
            .operations
            .iter()
            .position(|operation| matches!(operation, Operation::Rotate { .. }))
            .unwrap();
        let straighten_index = restored
            .operations
            .iter()
            .position(|operation| matches!(operation, Operation::Straighten { .. }))
            .unwrap();
        assert_eq!(straighten_index, rotate_index + 1);
    }

    #[test]
    fn version_twelve_project_gets_disabled_matrix_after_saturation() {
        let directory = tempdir().unwrap();
        let project_path = directory.path().join("version-twelve.focusless");
        let image_path = directory.path().join("image.jpg");
        let mut document = ProjectDocument::new(source(image_path));
        document.schema_version = 12;
        document
            .operations
            .retain(|operation| !matches!(operation, Operation::Matrix { .. }));
        fs::write(&project_path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

        let restored = load_project(&project_path).unwrap();

        assert_eq!(restored.schema_version, PROJECT_SCHEMA_VERSION);
        assert!(!restored.matrix_enabled());
        let saturation_index = restored
            .operations
            .iter()
            .position(|operation| matches!(operation, Operation::Saturation { .. }))
            .unwrap();
        let matrix_index = restored
            .operations
            .iter()
            .position(|operation| matches!(operation, Operation::Matrix { .. }))
            .unwrap();
        let sharpness_index = restored
            .operations
            .iter()
            .position(|operation| matches!(operation, Operation::Sharpness { .. }))
            .unwrap();
        assert!(saturation_index < matrix_index && matrix_index < sharpness_index);
    }

    #[test]
    fn version_thirteen_project_gets_neutral_shadows_highlights_in_render_order() {
        let directory = tempdir().unwrap();
        let project_path = directory.path().join("version-thirteen.focusless");
        let image_path = directory.path().join("image.jpg");
        let mut document = ProjectDocument::new(source(image_path));
        document.schema_version = 13;
        document
            .operations
            .retain(|operation| !matches!(operation, Operation::ShadowsHighlights { .. }));
        fs::write(&project_path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

        let restored = load_project(&project_path).unwrap();

        assert_eq!(restored.schema_version, PROJECT_SCHEMA_VERSION);
        assert_eq!(
            restored.shadows_highlights(),
            focusless_core::ShadowsHighlights::IDENTITY
        );
        let contrast_index = restored
            .operations
            .iter()
            .position(|operation| matches!(operation, Operation::Contrast { .. }))
            .unwrap();
        let adjustment_index = restored
            .operations
            .iter()
            .position(|operation| matches!(operation, Operation::ShadowsHighlights { .. }))
            .unwrap();
        let tone_curve_index = restored
            .operations
            .iter()
            .position(|operation| matches!(operation, Operation::ToneCurve { .. }))
            .unwrap();
        assert!(contrast_index < adjustment_index && adjustment_index < tone_curve_index);
    }

    #[test]
    fn version_fourteen_project_preserves_shadows_highlights_appearance() {
        let directory = tempdir().unwrap();
        let project_path = directory.path().join("version-fourteen.focusless");
        let image_path = directory.path().join("image.jpg");
        let mut document = ProjectDocument::new(source(image_path));
        document.schema_version = 14;
        document
            .commit_shadows_highlights(
                focusless_core::ShadowsHighlights::IDENTITY,
                focusless_core::ShadowsHighlights {
                    shadows: 25.0,
                    highlights: 40.0,
                },
            )
            .unwrap();
        fs::write(&project_path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

        let mut restored = load_project(&project_path).unwrap();

        assert_eq!(restored.schema_version, PROJECT_SCHEMA_VERSION);
        assert_eq!(
            restored.shadows_highlights(),
            focusless_core::ShadowsHighlights {
                shadows: -25.0,
                highlights: -40.0,
            }
        );
        assert!(restored.undo());
        assert_eq!(
            restored.shadows_highlights(),
            focusless_core::ShadowsHighlights::IDENTITY
        );
        assert!(restored.redo());
        assert_eq!(
            restored.shadows_highlights(),
            focusless_core::ShadowsHighlights {
                shadows: -25.0,
                highlights: -40.0,
            }
        );
    }

    #[test]
    fn failed_temp_file_is_discoverable_without_replacing_project() {
        let directory = tempdir().unwrap();
        let project_path = directory.path().join("edit.focusless");
        fs::write(&project_path, b"known-good").unwrap();
        let temp_path = temporary_path_for(&project_path);
        fs::write(&temp_path, b"partial").unwrap();

        assert_eq!(recovery_candidate(&project_path), Some(temp_path));
        assert_eq!(fs::read(&project_path).unwrap(), b"known-good");
    }

    #[test]
    fn sampled_fingerprint_changes_with_content() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("source.bin");
        fs::write(&path, b"abc").unwrap();
        let before = fingerprint_source(&path, 10, 20).unwrap();
        fs::write(&path, b"abd").unwrap();
        let after = fingerprint_source(&path, 10, 20).unwrap();
        assert_ne!(before.sample_blake3, after.sample_blake3);
    }
}
