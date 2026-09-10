//! Versioned, atomic persistence of recording configurations under
//! `~/Library/Application Support/Oculi`.

use crate::model::{RecordingConfig, SourceAvailability};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Current on-disk schema version. Bump when `PersistedSettings` changes in
/// a way that requires migration.
pub const CURRENT_VERSION: u32 = 1;

/// Root persisted document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSettings {
    pub version: u32,
    pub recordings: Vec<RecordingConfig>,
}

impl Default for PersistedSettings {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            recordings: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("failed to (de)serialize settings: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("unsupported settings version: {0}")]
    UnsupportedVersion(u32),
}

/// Returns `~/Library/Application Support/Oculi/settings.json`, honoring the
/// `HOME` env var so it is testable without touching the real user profile.
pub fn default_settings_path() -> PathBuf {
    default_settings_dir().join("settings.json")
}

pub fn default_settings_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Library")
        .join("Application Support")
        .join("Oculi")
}

/// Loads settings from `path`. Returns `PersistedSettings::default()` if the
/// file does not exist yet (first launch). Persisted source availability is
/// reset to `Unknown` for discovery to revalidate; source selections are kept.
/// The app is responsible for requiring explicit reselection of saved windows.
pub fn load_settings(path: &Path) -> Result<PersistedSettings, StorageError> {
    if !path.exists() {
        return Ok(PersistedSettings::default());
    }
    let contents = std::fs::read_to_string(path)?;
    let settings: PersistedSettings = serde_json::from_str(&contents)?;
    let mut settings = migrate(settings)?;
    for recording in &mut settings.recordings {
        recording.video_availability = SourceAvailability::Unknown;
        recording.audio_availability = SourceAvailability::Unknown;
    }
    Ok(settings)
}

/// Applies forward migrations. Currently a no-op beyond version validation
/// since `CURRENT_VERSION` is 1, but structured so future schema bumps have
/// a single place to add conversions.
fn migrate(settings: PersistedSettings) -> Result<PersistedSettings, StorageError> {
    if settings.version != CURRENT_VERSION {
        return Err(StorageError::UnsupportedVersion(settings.version));
    }
    Ok(settings)
}

/// Atomically writes `settings` to `path`: serialize to a unique, exclusively
/// created sibling temp file, sync it, then rename over the destination. Failed
/// saves attempt cleanup of only their own temp file. Concurrent saves are
/// last-rename-wins. Renaming within
/// the same filesystem is atomic on APFS, avoiding a torn/partial write if
/// the process is interrupted mid-save.
pub fn save_settings(path: &Path, settings: &PersistedSettings) -> Result<(), StorageError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(settings)?;
    let tmp_path = tmp_path_for(path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)?;
    let result = (|| -> io::Result<()> {
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp_path, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result.map_err(StorageError::Io)
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings.json".to_string());
    let tmp_name = format!(".{}.{}.tmp", file_name, Uuid::new_v4());
    path.with_file_name(tmp_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RecordingConfig, VideoSourceSelector};

    #[test]
    fn rejects_undefined_older_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"version": 0, "recordings": []}"#).unwrap();
        assert!(matches!(
            load_settings(&path),
            Err(StorageError::UnsupportedVersion(0))
        ));
    }

    #[test]
    fn load_resets_availability_without_changing_configuration() {
        use crate::model::{AudioSourceSelector, SourceAvailability, WindowId};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut recording = RecordingConfig::new("Window");
        recording.video = VideoSourceSelector::Window {
            window_id: WindowId(42),
            owner_bundle_id: None,
            title: "Saved window".into(),
        };
        recording.audio = AudioSourceSelector::System;
        recording.video_availability = SourceAvailability::Available;
        recording.audio_availability = SourceAvailability::Unavailable;
        save_settings(
            &path,
            &PersistedSettings {
                recordings: vec![recording.clone()],
                ..Default::default()
            },
        )
        .unwrap();
        recording.video_availability = SourceAvailability::Unknown;
        recording.audio_availability = SourceAvailability::Unknown;
        assert_eq!(load_settings(&path).unwrap().recordings, vec![recording]);
    }

    #[test]
    fn save_preserves_preexisting_predictable_temp_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let old_temp = dir
            .path()
            .join(format!(".settings.json.{}.tmp", std::process::id()));
        std::fs::write(&old_temp, b"keep this data").unwrap();
        save_settings(&path, &PersistedSettings::default()).unwrap();
        assert_eq!(std::fs::read(&old_temp).unwrap(), b"keep this data");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn concurrent_saves_leave_valid_json_without_temp_debris() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|index| {
                    let path = &path;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        let settings = PersistedSettings {
                            recordings: vec![RecordingConfig::new(format!("Writer {index}"))],
                            ..Default::default()
                        };
                        barrier.wait();
                        for _ in 0..20 {
                            save_settings(path, &settings).unwrap();
                            let json = std::fs::read(path).unwrap();
                            let loaded: PersistedSettings = serde_json::from_slice(&json).unwrap();
                            assert_eq!(loaded.version, CURRENT_VERSION);
                            assert_eq!(loaded.recordings.len(), 1);
                            assert!(loaded.recordings[0].name.starts_with("Writer "));
                        }
                    })
                })
                .collect();
            for handle in handles {
                handle.join().unwrap();
            }
        });
        assert_eq!(load_settings(&path).unwrap().recordings.len(), 1);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_rename_removes_owned_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::create_dir(&path).unwrap();
        let sentinel = path.join("keep");
        std::fs::write(&sentinel, b"keep this data").unwrap();
        assert!(save_settings(&path, &PersistedSettings::default()).is_err());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep this data");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let loaded = load_settings(&path).unwrap();
        assert_eq!(loaded.version, CURRENT_VERSION);
        assert!(loaded.recordings.is_empty());
    }

    #[test]
    fn save_then_load_round_trips_recordings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        let mut settings = PersistedSettings::default();
        let mut recording = RecordingConfig::new("Webcam");
        recording.video = VideoSourceSelector::Camera {
            device_id: crate::model::DeviceId("cam-42".into()),
            label: "Logi Webcam".into(),
        };
        settings.recordings.push(recording.clone());

        save_settings(&path, &settings).unwrap();
        let loaded = load_settings(&path).unwrap();

        assert_eq!(loaded.recordings.len(), 1);
        assert_eq!(loaded.recordings[0].name, "Webcam");
        assert_eq!(loaded.recordings[0].video, recording.video);
    }

    #[test]
    fn save_is_atomic_and_leaves_no_tmp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        save_settings(&path, &PersistedSettings::default()).unwrap();

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].to_string_lossy(), "settings.json");
    }

    #[test]
    fn rejects_future_unsupported_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"version": 999, "recordings": []}"#).unwrap();

        let result = load_settings(&path);
        assert!(matches!(result, Err(StorageError::UnsupportedVersion(999))));
    }

    #[test]
    fn default_settings_dir_uses_home_env_var() {
        let dir = default_settings_dir();
        assert!(dir.ends_with("Library/Application Support/Oculi"));
    }
}
