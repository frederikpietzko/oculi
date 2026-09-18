//! Testable configuration state and background persistence for the native UI.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};

use crate::capture::service::{CaptureSnapshot, CaptureState, CaptureTarget};
use crate::capture::{CaptureFormat, CaptureRequest, SourceKey};
use crate::discovery::SourceCatalog;
use crate::model::{
    GroupState, OutputSettings, RecordingConfig, RecordingId, Resolution, SourceAvailability,
    VideoSourceSelector,
};
use crate::recording::coordinator::GroupSnapshot;
use crate::storage::{PersistedSettings, ensure_writable_directory, load_settings, save_settings};

pub struct AppState {
    pub settings: PersistedSettings,
    pub selected: Option<RecordingId>,
    pub catalog: SourceCatalog,
    pub message: String,
    pub can_save: bool,
    pub dirty: bool,
    pub capture: CaptureSnapshot,
    pub group: GroupSnapshot,
    start_pending: bool,
    start_sequence: Option<u64>,
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
            group: GroupSnapshot::default(),
            start_pending: false,
            start_sequence: None,
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
        if !self.configuration_editable() {
            return;
        }
        let recording =
            RecordingConfig::new(format!("Recording {}", self.settings.recordings.len() + 1));
        self.selected = Some(recording.id);
        self.settings.recordings.push(recording);
        self.resolve_resolution_defaults();
        self.dirty = true;
    }

    pub fn duplicate(&mut self) {
        if !self.configuration_editable() {
            return;
        }
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
        self.require_configuration_editable()?;
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
        self.resolve_resolution_defaults();
        Ok(())
    }

    pub fn remove_confirmed(&mut self, id: RecordingId) {
        if !self.configuration_editable() {
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
        self.resolve_resolution_defaults();
    }

    pub fn remove(&mut self) {
        if !self.configuration_editable() {
            return;
        }
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
        self.resolve_resolution_defaults();
    }

    pub fn replace_selected(&mut self, mut recording: RecordingConfig, video_reselected: bool) {
        if !self.configuration_editable() {
            return;
        }
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

    pub fn configuration_locked(&self) -> bool {
        self.start_pending || self.group.active()
    }

    pub fn configuration_editable(&self) -> bool {
        self.can_save && !self.configuration_locked()
    }

    fn require_configuration_editable(&self) -> Result<(), String> {
        if self.configuration_locked() {
            Err("Source and output settings are locked until Stop & Save finishes. Framing remains editable.".into())
        } else if !self.can_save {
            Err("Configuration editing is unavailable until settings are repaired.".into())
        } else {
            Ok(())
        }
    }

    pub fn apply_output(
        &mut self,
        id: RecordingId,
        mut output: OutputSettings,
    ) -> Result<(), String> {
        self.require_configuration_editable()?;
        output.validate_format()?;
        if !output.directory.is_absolute() {
            return Err("Choose an output directory or enter its full absolute path.".into());
        }
        let recording = self
            .settings
            .recordings
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or("This recording no longer exists.")?;
        if output.resolution != recording.output.resolution {
            output.resolution_default_pending = false;
        }
        if recording.output != output {
            recording.output = output;
            self.dirty = true;
        }
        Ok(())
    }

    pub fn start_block_reason(&self) -> Option<String> {
        if let Err(error) = self.require_configuration_editable() {
            return Some(error);
        }
        if self.settings.recordings.is_empty() {
            return Some("Add a video and audio source before starting.".into());
        }
        for recording in &self.settings.recordings {
            if SourceKey::video(&recording.video).is_none()
                || SourceKey::audio(&recording.audio).is_none()
                || recording.video_availability != SourceAvailability::Available
                || recording.audio_availability != SourceAvailability::Available
            {
                return Some(format!(
                    "{}: choose available video and audio sources; check permissions in Settings.",
                    recording.name
                ));
            }
        }
        None
    }

    pub fn begin_group_start(&mut self) -> Result<Vec<RecordingConfig>, String> {
        if let Some(error) = self.start_block_reason() {
            return Err(error);
        }
        self.start_pending = true;
        self.start_sequence = None;
        Ok(self.settings.recordings.clone())
    }

    pub fn accept_group_start(&mut self, sequence: u64) {
        if self.start_pending {
            self.start_sequence = Some(sequence);
        }
    }

    pub fn reject_group_start(&mut self, error: String) {
        self.start_pending = false;
        self.start_sequence = None;
        self.message = error;
    }

    pub fn update_group(&mut self, snapshot: GroupSnapshot) {
        // State alone cannot acknowledge Start: it may belong to a previous run.
        if self
            .start_sequence
            .is_some_and(|sequence| snapshot.command_sequence >= sequence)
        {
            self.start_pending = false;
            self.start_sequence = None;
        }
        self.group = snapshot;
    }

    pub fn effective_group_state(&self) -> GroupState {
        if self.start_pending {
            GroupState::Preparing
        } else {
            self.group.state
        }
    }

    pub fn group_summary(&self) -> String {
        let seconds = self.group.elapsed.max(0.0) as u64;
        let elapsed = format!(
            "{:02}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        );
        let status = match self.effective_group_state() {
            GroupState::Idle => "Idle",
            GroupState::Preparing => "Preparing — checking sources and destinations",
            GroupState::Recording => "Recording",
            GroupState::Paused => "Paused",
            GroupState::Finalizing => "Finalizing — saving files; please wait",
            GroupState::Failed => "Failed — review output errors",
        };
        format!("{status} · {elapsed}")
    }

    pub fn recording_summary(&self, id: RecordingId) -> String {
        let Some(report) = self.group.recordings.iter().find(|report| report.id == id) else {
            return self.capture_summary(id);
        };
        let mut summary = format!(
            "{} · Audio {:.0}% · Dropped {} · Queues V:{} A:{}",
            report.status,
            report.audio_level.clamp(0.0, 1.0) * 100.0,
            report.dropped_video,
            report.video_queue,
            report.audio_queue
        );
        if let Some(path) = &report.saved_path {
            summary.push_str(&format!("\nSaved: {}", path.display()));
        }
        if let Some(error) = &report.error {
            summary.push_str(&format!("\nError: {error}"));
        }
        summary
    }

    /// First configured video wins, even when unavailable; never substitute a later source.
    pub fn first_source_resolution(&self) -> Option<Resolution> {
        let first = self
            .settings
            .recordings
            .iter()
            .find(|recording| !matches!(recording.video, VideoSourceSelector::None))?;
        if self.restored_windows.contains(&first.id) {
            return None;
        }
        self.catalog.resolutions(&first.video).first().copied()
    }

    pub fn resolution_choices(&self) -> Vec<Resolution> {
        let mut sizes = vec![
            Resolution {
                width: 640,
                height: 480,
            },
            Resolution {
                width: 1280,
                height: 720,
            },
            Resolution {
                width: 1920,
                height: 1080,
            },
            Resolution {
                width: 2560,
                height: 1440,
            },
            Resolution {
                width: 3840,
                height: 2160,
            },
        ];
        if let Some(first) = self
            .settings
            .recordings
            .iter()
            .find(|recording| !matches!(recording.video, VideoSourceSelector::None))
            && !self.restored_windows.contains(&first.id)
        {
            sizes.extend(self.catalog.resolutions(&first.video));
        }
        if let Some(selected) = self.selected() {
            if !self.restored_windows.contains(&selected.id) {
                sizes.extend(self.catalog.resolutions(&selected.video));
            }
            sizes.push(selected.output.resolution);
        }
        crate::discovery::sort_resolutions(&mut sizes);
        sizes
    }

    fn resolve_resolution_defaults(&mut self) -> bool {
        if !self.configuration_editable() {
            return false;
        }
        let Some(resolution) = self.first_source_resolution() else {
            return false;
        };
        let mut changed = false;
        for recording in &mut self.settings.recordings {
            if recording.output.resolution_default_pending {
                recording.output.resolution = resolution;
                recording.output.resolution_default_pending = false;
                changed = true;
            }
        }
        self.dirty |= changed;
        changed
    }

    pub fn update_catalog(&mut self, catalog: SourceCatalog) -> bool {
        self.catalog = catalog;
        for recording in &mut self.settings.recordings {
            self.catalog.revalidate(recording);
            if self.restored_windows.contains(&recording.id) {
                recording.video_availability = SourceAvailability::Unavailable;
            }
        }
        self.resolve_resolution_defaults()
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
                // Capture voice as mono; the AAC writer centers it in stereo.
                // Forcing stereo here can leave single-input devices left-only.
                let channels = if matches!(source, SourceKey::Microphone(_)) {
                    1
                } else {
                    2
                };
                targets.push(CaptureTarget {
                    recording: recording.id,
                    request: CaptureRequest {
                        source,
                        format: CaptureFormat::Audio {
                            sample_rate: 48_000,
                            channels,
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
                "Video + audio capture active · live preview".into()
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
    fn restored_application_process_requires_separate_video_and_audio_reselection() {
        use crate::model::{ApplicationProcessId, AudioSourceSelector, DisplayId};

        let process = ApplicationProcessId {
            pid: 42,
            start_time: [123, 456],
            session: uuid::Uuid::from_u128(1),
        };
        let mut live = RecordingConfig::new("Process");
        live.video = VideoSourceSelector::ApplicationProcess {
            process,
            display_id: DisplayId(1),
            label: "App".into(),
        };
        live.audio = AudioSourceSelector::ApplicationProcess {
            process,
            label: "App".into(),
        };
        let catalog = SourceCatalog {
            videos: vec![live.video.clone()],
            audios: vec![live.audio.clone()],
            ..Default::default()
        };
        let settings = PersistedSettings {
            recordings: vec![live.clone()],
            ..Default::default()
        };
        let restored = serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        let mut state = AppState::from_settings(restored);
        for _ in 0..2 {
            state.update_catalog(catalog.clone());
            assert_eq!(
                state.selected().unwrap().video_availability,
                SourceAvailability::Unavailable
            );
            assert_eq!(
                state.selected().unwrap().audio_availability,
                SourceAvailability::Unavailable
            );
        }
        let unchanged = state.selected().unwrap().clone();
        state.apply_source_draft(unchanged, false, false).unwrap();
        assert_eq!(
            state.selected().unwrap().video_availability,
            SourceAvailability::Unavailable
        );
        assert_eq!(
            state.selected().unwrap().audio_availability,
            SourceAvailability::Unavailable
        );

        let mut draft = state.selected().unwrap().clone();
        draft.video = live.video.clone();
        state.apply_source_draft(draft, false, true).unwrap();
        assert_eq!(
            state.selected().unwrap().video_availability,
            SourceAvailability::Available
        );
        assert_eq!(
            state.selected().unwrap().audio_availability,
            SourceAvailability::Unavailable
        );
        let mut draft = state.selected().unwrap().clone();
        draft.audio = live.audio.clone();
        state.apply_source_draft(draft, false, false).unwrap();
        assert_eq!(
            state.selected().unwrap().audio_availability,
            SourceAvailability::Available
        );

        let restarted = ApplicationProcessId {
            start_time: [124, 456],
            ..process
        };
        state.update_catalog(SourceCatalog {
            videos: vec![VideoSourceSelector::ApplicationProcess {
                process: restarted,
                display_id: DisplayId(1),
                label: "App".into(),
            }],
            audios: vec![AudioSourceSelector::ApplicationProcess {
                process: restarted,
                label: "App".into(),
            }],
            ..Default::default()
        });
        assert_eq!(
            state.selected().unwrap().video_availability,
            SourceAvailability::Unavailable
        );
        assert_eq!(
            state.selected().unwrap().audio_availability,
            SourceAvailability::Unavailable
        );
        assert_eq!(state.selected().unwrap().video, live.video);
        assert_eq!(state.selected().unwrap().audio, live.audio);
    }

    fn resolution_catalog(first: &RecordingConfig, second: &RecordingConfig) -> SourceCatalog {
        SourceCatalog {
            videos: vec![second.video.clone(), first.video.clone()],
            video_resolutions: vec![
                (
                    second.video.clone(),
                    vec![Resolution {
                        width: 3840,
                        height: 2160,
                    }],
                ),
                (
                    first.video.clone(),
                    vec![
                        Resolution {
                            width: 1280,
                            height: 720,
                        },
                        Resolution {
                            width: 2560,
                            height: 1440,
                        },
                    ],
                ),
            ],
            ..Default::default()
        }
    }

    fn resolution_pair() -> (RecordingConfig, RecordingConfig) {
        let mut first = RecordingConfig::new("First");
        first.video = VideoSourceSelector::Camera {
            device_id: crate::model::DeviceId("first".into()),
            label: "Camera".into(),
        };
        let mut second = RecordingConfig::new("Second");
        second.video = VideoSourceSelector::Camera {
            device_id: crate::model::DeviceId("second".into()),
            label: "Camera".into(),
        };
        (first, second)
    }

    #[test]
    fn new_recordings_default_to_first_configured_video_not_discovery_or_selection_order() {
        let (first, second) = resolution_pair();
        let mut state = AppState::from_settings(PersistedSettings::default());
        state.update_catalog(resolution_catalog(&first, &second));
        state.apply_source_draft(first, true, true).unwrap();
        state
            .apply_source_draft(second.clone(), true, true)
            .unwrap();
        assert_eq!(state.selected, Some(second.id));
        let expected = Resolution {
            width: 2560,
            height: 1440,
        };
        assert_eq!(state.first_source_resolution(), Some(expected));
        for recording in &state.settings.recordings {
            assert_eq!(recording.output.resolution, expected);
            assert!(!recording.output.resolution_default_pending);
        }
        assert!(state.resolution_choices().contains(&Resolution {
            width: 3840,
            height: 2160
        }));
        state.duplicate();
        assert_eq!(state.selected().unwrap().output.resolution, expected);
    }

    #[test]
    fn delayed_discovery_resolves_once_and_never_falls_back_to_a_later_source() {
        let (first, second) = resolution_pair();
        let catalog = resolution_catalog(&first, &second);
        let mut state = AppState::from_settings(PersistedSettings {
            recordings: vec![first.clone(), second],
            ..Default::default()
        });
        let mut missing = catalog.clone();
        missing
            .videos
            .retain(|video| !crate::discovery::same_video(video, &first.video));
        assert!(!state.update_catalog(missing));
        assert_eq!(state.first_source_resolution(), None);
        assert!(
            state
                .settings
                .recordings
                .iter()
                .all(|r| r.output.resolution_default_pending)
        );
        assert!(state.update_catalog(catalog.clone()));
        let outputs: Vec<_> = state
            .settings
            .recordings
            .iter()
            .map(|r| r.output.clone())
            .collect();
        let mut resized = catalog;
        resized.video_resolutions[1].1 = vec![Resolution {
            width: 4096,
            height: 2160,
        }];
        assert!(!state.update_catalog(resized));
        assert_eq!(
            outputs,
            state
                .settings
                .recordings
                .iter()
                .map(|r| r.output.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn custom_and_legacy_resolutions_survive_discovery_and_round_trip() {
        let (mut first, mut second) = resolution_pair();
        first.output.resolution_default_pending = false;
        first.output.resolution = Resolution {
            width: 1000,
            height: 560,
        };
        second.output.directory = std::env::current_dir().unwrap();
        let catalog = resolution_catalog(&first, &second);
        let mut state = AppState::from_settings(PersistedSettings {
            recordings: vec![first, second.clone()],
            ..Default::default()
        });
        let mut custom = second.output.clone();
        custom.resolution = Resolution {
            width: 1600,
            height: 900,
        };
        state.apply_output(second.id, custom).unwrap();
        assert!(!state.update_catalog(catalog));
        assert_eq!(
            state.settings.recordings[0].output.resolution,
            Resolution {
                width: 1000,
                height: 560
            }
        );
        assert_eq!(
            state.settings.recordings[1].output.resolution,
            Resolution {
                width: 1600,
                height: 900
            }
        );
        let path = tempfile::tempdir().unwrap();
        save_settings(&path.path().join("settings.json"), &state.settings).unwrap();
        let restored = AppState::load(&path.path().join("settings.json"));
        for recording in restored.settings.recordings {
            assert!(!recording.output.resolution_default_pending);
        }
    }

    #[test]
    fn discovery_never_changes_resolution_during_group_or_pending_start() {
        for group in [
            GroupState::Preparing,
            GroupState::Recording,
            GroupState::Paused,
            GroupState::Finalizing,
        ] {
            let (first, second) = resolution_pair();
            let catalog = resolution_catalog(&first, &second);
            let mut state = AppState::from_settings(PersistedSettings {
                recordings: vec![first, second],
                ..Default::default()
            });
            state.group.state = group;
            assert!(!state.update_catalog(catalog.clone()));
            assert!(
                state
                    .settings
                    .recordings
                    .iter()
                    .all(|r| r.output.resolution == Resolution::default())
            );
            state.group.state = GroupState::Idle;
            state.start_pending = true;
            assert!(!state.update_catalog(catalog.clone()));
            state.start_pending = false;
            assert!(state.update_catalog(catalog));
        }
    }

    #[test]
    fn unchanged_output_edits_keep_pending_but_explicit_same_size_override_clears_it() {
        let (first, second) = resolution_pair();
        let mut state = AppState::from_settings(PersistedSettings {
            recordings: vec![first.clone()],
            ..Default::default()
        });
        let mut output = first.output;
        output.directory = std::env::current_dir().unwrap();
        state.apply_output(first.id, output.clone()).unwrap();
        assert!(state.selected().unwrap().output.resolution_default_pending);
        output.resolution_default_pending = false;
        state.apply_output(first.id, output).unwrap();
        assert!(!state.update_catalog(resolution_catalog(state.selected().unwrap(), &second)));
        assert_eq!(
            state.selected().unwrap().output.resolution,
            Resolution::default()
        );
    }

    fn ready_state() -> AppState {
        let mut state = AppState::from_settings(PersistedSettings::default());
        state.add();
        let recording = &mut state.settings.recordings[0];
        recording.video = VideoSourceSelector::Display {
            display_id: crate::model::DisplayId(1),
            label: "Display".into(),
        };
        recording.audio = crate::model::AudioSourceSelector::System;
        recording.video_availability = SourceAvailability::Available;
        recording.audio_availability = SourceAvailability::Available;
        state.dirty = false;
        state
    }

    #[test]
    fn restored_window_needs_reselection_before_its_size_can_be_a_default() {
        let mut first = RecordingConfig::new("Window");
        first.video = VideoSourceSelector::Window {
            window_id: WindowId(42),
            owner_bundle_id: None,
            title: "Window".into(),
        };
        let (_, second) = resolution_pair();
        let catalog = resolution_catalog(&first, &second);
        let mut state = AppState::from_settings(PersistedSettings {
            recordings: vec![first.clone(), second],
            ..Default::default()
        });
        assert!(!state.update_catalog(catalog));
        assert_eq!(state.first_source_resolution(), None);
        state.apply_source_draft(first, false, true).unwrap();
        assert_eq!(
            state.first_source_resolution(),
            Some(Resolution {
                width: 2560,
                height: 1440
            })
        );
        assert!(
            state
                .settings
                .recordings
                .iter()
                .all(|r| !r.output.resolution_default_pending)
        );
    }

    #[test]
    fn deleting_first_source_resolves_only_pending_outputs_from_the_new_first_source() {
        let (first, second) = resolution_pair();
        let mut catalog = resolution_catalog(&first, &second);
        catalog
            .videos
            .retain(|video| !crate::discovery::same_video(video, &first.video));
        let mut state = AppState::from_settings(PersistedSettings {
            recordings: vec![first.clone(), second],
            ..Default::default()
        });
        state.update_catalog(catalog);
        state.remove_confirmed(first.id);
        assert_eq!(
            state.selected().unwrap().output.resolution,
            Resolution {
                width: 3840,
                height: 2160
            }
        );
        assert!(!state.selected().unwrap().output.resolution_default_pending);
    }

    fn assert_configuration_locked(state: &mut AppState) {
        let original = state.settings.recordings.clone();
        let selection = state.selected;
        let mut draft = original[0].clone();
        draft.name = "Must not replace".into();
        state.add();
        state.duplicate();
        state.remove();
        state.remove_confirmed(draft.id);
        state.replace_selected(draft.clone(), true);
        assert!(
            state
                .apply_source_draft(draft.clone(), false, true)
                .is_err()
        );
        assert!(
            state
                .apply_source_draft(draft.duplicate(), true, true)
                .is_err()
        );
        draft.output.fps = 60;
        assert!(state.apply_output(draft.id, draft.output).is_err());
        assert!(state.begin_group_start().is_err());
        assert_eq!(state.settings.recordings, original);
        assert_eq!(state.selected, selection);
        assert!(!state.dirty);
    }

    #[test]
    fn all_configuration_mutations_lock_for_every_active_group_state() {
        for group in [
            GroupState::Preparing,
            GroupState::Recording,
            GroupState::Paused,
            GroupState::Finalizing,
        ] {
            let mut state = ready_state();
            state.update_group(GroupSnapshot {
                state: group,
                ..Default::default()
            });
            assert_configuration_locked(&mut state);
            state.update_group(GroupSnapshot::default());
            state.duplicate();
            assert_eq!(state.settings.recordings.len(), 2);
        }
    }

    #[test]
    fn queued_start_locks_before_backend_ack_and_idle_does_not_unlock() {
        let mut state = ready_state();
        let configs = state.begin_group_start().unwrap();
        assert_eq!(configs, state.settings.recordings);
        assert_eq!(state.effective_group_state(), GroupState::Preparing);
        state.update_group(GroupSnapshot::default());
        assert_configuration_locked(&mut state);
        state.accept_group_start(1);
        state.update_group(GroupSnapshot {
            state: GroupState::Preparing,
            command_sequence: 1,
            ..Default::default()
        });
        assert_configuration_locked(&mut state);
        state.update_group(GroupSnapshot {
            state: GroupState::Failed,
            error: Some("Permission denied".into()),
            command_sequence: 1,
            ..Default::default()
        });
        assert!(state.configuration_editable());
        assert!(state.begin_group_start().is_ok());
        state.reject_group_start("Command queue full; retry Start.".into());
        assert!(state.configuration_editable());
        assert!(state.message.contains("retry Start"));
    }

    #[test]
    fn retried_start_waits_for_its_sequence_not_stale_state_or_unrelated_ack() {
        for acknowledged_state in [GroupState::Failed, GroupState::Recording, GroupState::Idle] {
            for acknowledged_sequence in [12, 13] {
                let mut state = ready_state();
                let failed = GroupSnapshot {
                    state: GroupState::Failed,
                    command_sequence: 10,
                    ..Default::default()
                };
                state.update_group(failed.clone());
                state.begin_group_start().unwrap();
                // Even polling before enqueue returns must preserve the local lock.
                state.update_group(failed.clone());
                assert_configuration_locked(&mut state);
                state.accept_group_start(12);
                for stale in [
                    failed,
                    GroupSnapshot {
                        state: GroupState::Idle,
                        command_sequence: 11,
                        ..Default::default()
                    },
                ] {
                    state.update_group(stale);
                    assert_eq!(state.effective_group_state(), GroupState::Preparing);
                    assert_configuration_locked(&mut state);
                }
                state.update_group(GroupSnapshot {
                    state: acknowledged_state,
                    command_sequence: acknowledged_sequence,
                    ..Default::default()
                });
                assert!(!state.start_pending);
                assert_eq!(state.start_sequence, None);
                assert_eq!(state.effective_group_state(), acknowledged_state);
                assert_eq!(
                    state.configuration_locked(),
                    acknowledged_state == GroupState::Recording
                );
            }
        }
    }

    #[test]
    fn start_requires_settings_and_real_available_source_selections() {
        let mut state = AppState::from_settings(PersistedSettings::default());
        assert!(state.begin_group_start().is_err());
        state.add();
        assert!(state.begin_group_start().is_err());
        let mut state = ready_state();
        state.settings.recordings[0].audio_availability = SourceAvailability::Unknown;
        assert!(state.begin_group_start().is_err());
        state.settings.recordings[0].audio_availability = SourceAvailability::Available;
        state.can_save = false;
        assert_configuration_locked(&mut state);
        state.can_save = true;
        assert!(state.begin_group_start().is_ok());
    }

    #[test]
    fn output_edits_are_transactional_and_do_not_replace_sources_or_framing() {
        let mut state = ready_state();
        let original = state.selected().unwrap().clone();
        let mut output = original.output.clone();
        output.resolution.width = 1921;
        assert!(state.apply_output(original.id, output.clone()).is_err());
        assert_eq!(state.selected(), Some(&original));
        output.resolution.width = 1920;
        output.directory = PathBuf::from("/tmp/oculi-output-test");
        output.fps = 60;
        state.apply_output(original.id, output.clone()).unwrap();
        let current = state.selected().unwrap();
        assert_eq!(current.output, output);
        assert_eq!(current.video, original.video);
        assert_eq!(current.audio, original.audio);
        assert_eq!(current.viewport, original.viewport);
    }

    #[test]
    fn group_reports_keep_saved_paths_failures_and_diagnostics_after_finalization() {
        let mut state = ready_state();
        let recording = state.selected().unwrap();
        let report = crate::recording::coordinator::RecordingReport {
            id: recording.id,
            name: recording.name.clone(),
            status: "Saved with warning".into(),
            saved_path: Some(PathBuf::from("/tmp/output.mp4")),
            error: Some("Audio ended early".into()),
            audio_level: 0.5,
            dropped_video: 12,
            video_queue: 2,
            audio_queue: 3,
        };
        let id = report.id;
        state.update_group(GroupSnapshot {
            elapsed: 3661.0,
            recordings: vec![report],
            ..Default::default()
        });
        let summary = state.recording_summary(id);
        for text in [
            "Saved: /tmp/output.mp4",
            "Audio ended early",
            "Audio 50%",
            "Dropped 12",
            "V:2 A:3",
        ] {
            assert!(summary.contains(text), "{summary}");
        }
        assert!(state.group_summary().contains("01:01:01"));
        assert!(!state.configuration_locked());
    }

    #[test]
    fn microphone_capture_is_mono_while_playback_remains_stereo() {
        use crate::model::{AudioSourceSelector, BundleId, DeviceId};

        let mut state = AppState::from_settings(PersistedSettings::default());
        state.add();
        for (audio, channels) in [
            (
                AudioSourceSelector::Microphone {
                    device_id: DeviceId("microphone".into()),
                    label: "Microphone".into(),
                },
                1,
            ),
            (
                AudioSourceSelector::Application {
                    bundle_id: BundleId("app".into()),
                    label: "Application".into(),
                },
                2,
            ),
            (AudioSourceSelector::System, 2),
        ] {
            let source = SourceKey::audio(&audio).unwrap();
            let recording = &mut state.settings.recordings[0];
            recording.audio = audio;
            recording.audio_availability = SourceAvailability::Available;
            let targets = state.capture_targets();
            assert_eq!(targets.len(), 1);
            assert_eq!(targets[0].request.source, source);
            assert_eq!(
                targets[0].request.format,
                CaptureFormat::Audio {
                    sample_rate: 48_000,
                    channels,
                }
            );
        }
    }

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
