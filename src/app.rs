//! Testable configuration state and background persistence for the native UI.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};

use crate::capture::service::{CaptureSnapshot, CaptureState, CaptureTarget};
use crate::capture::{CaptureFormat, CaptureRequest, SourceKey};
use crate::discovery::SourceCatalog;
use crate::model::{RecordingConfig, RecordingId, SourceAvailability, VideoSourceSelector};
use crate::storage::{PersistedSettings, ensure_writable_directory, load_settings, save_settings};

pub struct AppState {
    pub settings: PersistedSettings,
    pub selected: Option<RecordingId>,
    pub catalog: SourceCatalog,
    pub message: String,
    pub can_save: bool,
    pub dirty: bool,
    pub capture: CaptureSnapshot,
    restored_windows: HashSet<RecordingId>,
}

impl AppState {
    pub fn load(path: &std::path::Path) -> Self {
        match load_settings(path) {
            Ok(settings) => Self::from_settings(settings),
            Err(error) => {
                let mut state = Self::from_settings(PersistedSettings::default());
                state.can_save = false;
                state.message = format!(
                    "Settings could not be loaded: {error}. Original file preserved at {}. Repair it and restart Oculi.",
                    path.display()
                );
                state
            }
        }
    }

    pub fn from_settings(mut settings: PersistedSettings) -> Self {
        let restored_windows = settings
            .recordings
            .iter()
            .filter_map(|r| matches!(r.video, VideoSourceSelector::Window { .. }).then_some(r.id))
            .collect();
        for recording in &mut settings.recordings {
            recording.video_availability = SourceAvailability::Unknown;
            recording.audio_availability = SourceAvailability::Unknown;
        }
        let selected = settings.recordings.first().map(|r| r.id);
        Self {
            settings,
            selected,
            catalog: SourceCatalog::default(),
            message: String::new(),
            can_save: true,
            dirty: false,
            capture: CaptureSnapshot::default(),
            restored_windows,
        }
    }

    pub fn selected(&self) -> Option<&RecordingConfig> {
        self.settings
            .recordings
            .iter()
            .find(|r| Some(r.id) == self.selected)
    }

    pub fn add(&mut self) {
        let recording =
            RecordingConfig::new(format!("Recording {}", self.settings.recordings.len() + 1));
        self.selected = Some(recording.id);
        self.settings.recordings.push(recording);
        self.dirty = true;
    }

    pub fn duplicate(&mut self) {
        if let Some(original) = self.selected() {
            let copy = original.duplicate();
            if self.restored_windows.contains(&original.id) {
                self.restored_windows.insert(copy.id);
            }
            self.selected = Some(copy.id);
            self.settings.recordings.push(copy);
            self.dirty = true;
        }
    }

    /// Applies a source dialog only on explicit confirmation; drafts stay outside persisted state.
    pub fn apply_source_draft(
        &mut self,
        mut draft: RecordingConfig,
        is_new: bool,
        video_reselected: bool,
    ) -> Result<(), String> {
        if !self.can_save {
            return Err("Configuration editing is unavailable until settings are repaired.".into());
        }
        draft.name = draft.name.trim().into();
        if draft.name.is_empty() {
            return Err("Give this source a name.".into());
        }
        let existing = self.settings.recordings.iter().find(|r| r.id == draft.id);
        if is_new {
            if existing.is_some() {
                return Err("This source has already been added.".into());
            }
            self.catalog.revalidate(&mut draft);
            self.selected = Some(draft.id);
            self.settings.recordings.push(draft);
            self.dirty = true;
        } else {
            let existing = existing.ok_or("This source no longer exists.")?;
            // A source dialog edits only identity, video and audio, never output/framing settings.
            draft.output = existing.output.clone();
            draft.viewport = existing.viewport;
            self.replace_selected(draft, video_reselected);
        }
        Ok(())
    }

    pub fn remove_confirmed(&mut self, id: RecordingId) {
        if !self.can_save {
            return;
        }
        if let Some(index) = self.settings.recordings.iter().position(|r| r.id == id) {
            self.settings.recordings.remove(index);
            self.restored_windows.remove(&id);
            if self.selected == Some(id) {
                self.selected = self
                    .settings
                    .recordings
                    .get(index.min(self.settings.recordings.len().saturating_sub(1)))
                    .map(|r| r.id);
            }
            self.dirty = true;
        }
    }

    pub fn remove(&mut self) {
        if let Some(id) = self.selected {
            let index = self
                .settings
                .recordings
                .iter()
                .position(|r| r.id == id)
                .unwrap_or(0);
            self.settings.recordings.retain(|r| r.id != id);
            self.restored_windows.remove(&id);
            self.selected = self
                .settings
                .recordings
                .get(index.min(self.settings.recordings.len().saturating_sub(1)))
                .map(|r| r.id);
            self.dirty = true;
        }
    }

    pub fn replace_selected(&mut self, mut recording: RecordingConfig, video_reselected: bool) {
        if video_reselected {
            self.restored_windows.remove(&recording.id);
        }
        self.catalog.revalidate(&mut recording);
        if self.restored_windows.contains(&recording.id) {
            recording.video_availability = SourceAvailability::Unavailable;
        }
        if let Some(existing) = self
            .settings
            .recordings
            .iter_mut()
            .find(|r| r.id == recording.id)
            && *existing != recording
        {
            *existing = recording;
            self.dirty = true;
        }
    }

    pub fn update_catalog(&mut self, catalog: SourceCatalog) {
        self.catalog = catalog;
        for recording in &mut self.settings.recordings {
            self.catalog.revalidate(recording);
            if self.restored_windows.contains(&recording.id) {
                recording.video_availability = SourceAvailability::Unavailable;
            }
        }
    }

    pub fn needs_window_reselection(&self, id: RecordingId) -> bool {
        self.restored_windows.contains(&id)
    }

    pub fn capture_targets(&self) -> Vec<CaptureTarget> {
        let mut targets = Vec::new();
        for recording in &self.settings.recordings {
            if recording.output.validate_format().is_err() {
                continue;
            }
            if recording.video_availability == SourceAvailability::Available
                && let Some(source) = SourceKey::video(&recording.video)
            {
                targets.push(CaptureTarget {
                    recording: recording.id,
                    request: CaptureRequest {
                        source,
                        format: CaptureFormat::Video {
                            width: recording.output.resolution.width,
                            height: recording.output.resolution.height,
                            fps: recording.output.fps,
                        },
                    },
                });
            }
            if recording.audio_availability == SourceAvailability::Available
                && let Some(source) = SourceKey::audio(&recording.audio)
            {
                targets.push(CaptureTarget {
                    recording: recording.id,
                    request: CaptureRequest {
                        source,
                        format: CaptureFormat::Audio {
                            sample_rate: 48_000,
                            channels: 2,
                        },
                    },
                });
            }
        }
        targets
    }

    pub fn capture_summary(&self, id: RecordingId) -> String {
        let Some(recording) = self.settings.recordings.iter().find(|r| r.id == id) else {
            return String::new();
        };
        if let Err(error) = recording.output.validate_format() {
            return error;
        }
        let video = SourceKey::video(&recording.video);
        let audio = SourceKey::audio(&recording.audio);
        let states: Vec<_> = self
            .capture
            .statuses
            .iter()
            .filter(|s| {
                s.recording == id
                    && (Some(&s.source) == video.as_ref() || Some(&s.source) == audio.as_ref())
            })
            .collect();
        let errors: Vec<_> = states
            .iter()
            .filter_map(|s| match &s.state {
                CaptureState::Failed(error) => Some(error.to_string()),
                _ => None,
            })
            .collect();
        if !errors.is_empty() {
            return errors.join("; ");
        }
        if recording.video_availability != SourceAvailability::Available
            || recording.audio_availability != SourceAvailability::Available
        {
            return "Sources unavailable — check selection and permissions".into();
        }
        let receiving = states
            .iter()
            .filter(|s| matches!(s.state, CaptureState::Receiving { .. }))
            .count();
        let dropped: u64 = states
            .iter()
            .map(|s| match s.state {
                CaptureState::Receiving { dropped, .. } => dropped,
                _ => 0,
            })
            .sum();
        if receiving == 2 {
            if dropped > 0 {
                format!("Capture active · {dropped} skipped samples (preview only)")
            } else {
                "Video + audio capture active · preview coming next".into()
            }
        } else {
            format!("Waiting for source samples ({receiving}/2) · not recording")
        }
    }
}

pub struct SaveResult {
    pub error: Option<String>,
    pub destination_warnings: Vec<String>,
    pub wrote_settings: bool,
}

struct SettingsRequest {
    settings: PersistedSettings,
    write: bool,
}

/// A single bounded worker keeps filesystem probes and writes off AppKit.
pub struct SettingsWorker {
    requests: SyncSender<SettingsRequest>,
    pub results: Receiver<SaveResult>,
    pub pending: bool,
    pub failed: bool,
}

impl SettingsWorker {
    pub fn new(path: PathBuf) -> Self {
        let (requests, rx) = mpsc::sync_channel::<SettingsRequest>(1);
        let (tx, results) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            while let Ok(SettingsRequest { settings, write }) = rx.recv() {
                let error = if write {
                    save_settings(&path, &settings).err().map(|e| e.to_string())
                } else {
                    None
                };
                let destination_warnings = settings
                    .recordings
                    .iter()
                    .filter_map(|r| {
                        r.output
                            .validate_format()
                            .and_then(|()| {
                                ensure_writable_directory(&r.output.directory)
                                    .map_err(|e| e.to_string())
                            })
                            .err()
                            .map(|e| format!("{}: {e}", r.name))
                    })
                    .collect();
                if tx
                    .send(SaveResult {
                        error,
                        destination_warnings,
                        wrote_settings: write,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            requests,
            results,
            pending: false,
            failed: false,
        }
    }

    pub fn validate_restored(&mut self, settings: &PersistedSettings) {
        if !self.pending
            && self
                .requests
                .try_send(SettingsRequest {
                    settings: settings.clone(),
                    write: false,
                })
                .is_ok()
        {
            self.pending = true;
        }
    }

    pub fn save(&mut self, state: &mut AppState) {
        if !state.dirty || !state.can_save || self.pending {
            return;
        }
        match self.requests.try_send(SettingsRequest {
            settings: state.settings.clone(),
            write: true,
        }) {
            Ok(()) => {
                self.pending = true;
                self.failed = false;
                state.dirty = false;
            }
            Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => {
                state.message = "Settings worker stopped; changes are not saved. Restart Oculi after preserving your configuration.".into();
                state.can_save = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WindowId;

    #[test]
    fn capture_demand_excludes_unavailable_sources_without_substitution() {
        let mut state = AppState::from_settings(PersistedSettings::default());
        state.add();
        let r = &mut state.settings.recordings[0];
        r.video = VideoSourceSelector::Display {
            display_id: crate::model::DisplayId(12),
            label: "Display".into(),
        };
        r.audio = crate::model::AudioSourceSelector::System;
        r.video_availability = SourceAvailability::Available;
        r.audio_availability = SourceAvailability::Unavailable;
        let targets = state.capture_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].request.source, SourceKey::Display(12));
        state.settings.recordings[0].video_availability = SourceAvailability::Unavailable;
        assert!(state.capture_targets().is_empty());
        assert!(matches!(
            state.settings.recordings[0].video,
            VideoSourceSelector::Display { .. }
        ));
    }

    #[test]
    fn native_failures_are_visible_per_recording_and_ignore_obsolete_sources() {
        let mut state = AppState::from_settings(PersistedSettings::default());
        state.add();
        let r = &mut state.settings.recordings[0];
        let id = r.id;
        r.audio = crate::model::AudioSourceSelector::System;
        state
            .capture
            .statuses
            .push(crate::capture::service::CaptureStatus {
                recording: id,
                source: SourceKey::SystemAudio,
                state: CaptureState::Failed(crate::capture::CaptureError::Permission(
                    "Screen & System Audio Recording".into(),
                )),
            });
        assert!(state.capture_summary(id).contains("Permission required"));
        state.settings.recordings[0].audio = crate::model::AudioSourceSelector::None;
        assert!(!state.capture_summary(id).contains("Permission required"));
    }

    #[test]
    fn source_drafts_are_transactional_and_reject_invalid_names() {
        let mut state = AppState::from_settings(PersistedSettings::default());
        let mut draft = RecordingConfig::new("First");
        assert!(state.settings.recordings.is_empty());
        draft.name = "  ".into();
        assert!(
            state
                .apply_source_draft(draft.clone(), true, false)
                .is_err()
        );
        assert!(!state.dirty);
        draft.name = " First ".into();
        state
            .apply_source_draft(draft.clone(), true, false)
            .unwrap();
        assert_eq!(state.selected().unwrap().name, "First");
        assert!(state.apply_source_draft(draft, true, false).is_err());
        let mut canceled = state.selected().unwrap().clone();
        canceled.name = "Canceled edit".into();
        drop(canceled);
        assert_eq!(state.selected().unwrap().name, "First");
    }

    #[test]
    fn source_edit_preserves_output_and_cannot_recreate_deleted_target() {
        let mut state = AppState::from_settings(PersistedSettings::default());
        state.add();
        let mut draft = state.selected().unwrap().clone();
        let output = draft.output.clone();
        draft.name = "Edited".into();
        draft.output.fps = 60;
        state
            .apply_source_draft(draft.clone(), false, false)
            .unwrap();
        assert_eq!(state.selected().unwrap().output, output);
        assert_eq!(state.selected().unwrap().name, "Edited");
        state.remove_confirmed(draft.id);
        assert!(state.apply_source_draft(draft, false, false).is_err());
        assert!(state.settings.recordings.is_empty());
    }

    #[test]
    fn confirmation_removes_only_its_original_target() {
        let mut state = AppState::from_settings(PersistedSettings::default());
        state.add();
        let original = state.selected.unwrap();
        state.add();
        let current = state.selected.unwrap();
        state.remove_confirmed(original);
        assert_eq!(state.selected, Some(current));
        assert_eq!(state.settings.recordings.len(), 1);
        state.dirty = false;
        state.remove_confirmed(original);
        assert!(!state.dirty);
    }

    #[test]
    fn read_only_settings_reject_dialog_changes_and_deletion() {
        let mut state = AppState::from_settings(PersistedSettings::default());
        state.add();
        let draft = state.selected().unwrap().clone();
        state.can_save = false;
        assert!(
            state
                .apply_source_draft(draft.clone(), false, false)
                .is_err()
        );
        state.remove_confirmed(draft.id);
        assert_eq!(state.settings.recordings.len(), 1);
    }

    #[test]
    fn add_duplicate_remove_preserves_selection_and_identity() {
        let mut state = AppState::from_settings(PersistedSettings::default());
        state.add();
        let first = state.selected.unwrap();
        state.duplicate();
        assert_ne!(state.selected, Some(first));
        assert_eq!(state.settings.recordings.len(), 2);
        state.remove();
        assert_eq!(state.selected, Some(first));
        state.remove();
        assert!(state.selected.is_none());
    }

    #[test]
    fn restored_window_id_is_not_silently_reused() {
        let mut recording = RecordingConfig::new("Window");
        recording.video = VideoSourceSelector::Window {
            window_id: WindowId(42),
            owner_bundle_id: None,
            title: "Document".into(),
        };
        let mut state = AppState::from_settings(PersistedSettings {
            recordings: vec![recording.clone()],
            ..Default::default()
        });
        state.update_catalog(SourceCatalog {
            videos: vec![recording.video.clone()],
            ..Default::default()
        });
        assert_eq!(
            state.selected().unwrap().video_availability,
            SourceAvailability::Unavailable
        );
        state.duplicate();
        assert!(state.needs_window_reselection(state.selected.unwrap()));
        state.replace_selected(state.selected().unwrap().clone(), true);
        assert_eq!(
            state.selected().unwrap().video_availability,
            SourceAvailability::Available
        );
    }

    #[test]
    fn corrupt_settings_are_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"not json").unwrap();
        let state = AppState::load(&path);
        assert!(!state.can_save);
        assert_eq!(std::fs::read(&path).unwrap(), b"not json");
    }

    #[test]
    fn edits_during_background_save_remain_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut worker = SettingsWorker::new(path.clone());
        let mut state = AppState::from_settings(PersistedSettings::default());
        state.add();
        state.settings.recordings[0].output.directory = dir.path().join("movies");
        worker.save(&mut state);
        state.duplicate();
        worker.save(&mut state);
        assert!(state.dirty);
        assert!(
            worker
                .results
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .error
                .is_none()
        );
        worker.pending = false;
        worker.save(&mut state);
        assert!(
            worker
                .results
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .error
                .is_none()
        );
        assert_eq!(load_settings(&path).unwrap().recordings.len(), 2);
    }

    #[test]
    fn restore_validation_does_not_rewrite_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut worker = SettingsWorker::new(path.clone());
        let mut state = AppState::from_settings(PersistedSettings::default());
        state.add();
        state.settings.recordings[0].output.directory = dir.path().join("movies");
        worker.validate_restored(&state.settings);
        let result = worker
            .results
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(!result.wrote_settings);
        assert!(!path.exists());
        assert!(result.destination_warnings.is_empty());
    }
}
