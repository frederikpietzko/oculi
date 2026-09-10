//! Core domain model for Oculi recording configurations.
//!
//! These types are platform-agnostic (no AppKit / ScreenCaptureKit / AVFoundation
//! dependencies) so they can be unit-tested without macOS frameworks and shared
//! between the UI, storage and (future) capture layers.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// Stable identifier for a single recording configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RecordingId(pub Uuid);

impl RecordingId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for RecordingId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for RecordingId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Persistent identifier for a camera or microphone device, stable across
/// discovery refreshes (backed by `AVCaptureDevice.uniqueID`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub String);

/// Persistent identifier for an application, backed by its bundle identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BundleId(pub String);

/// Persistent identifier for a display, backed by `CGDirectDisplayID`.
///
/// Display IDs may change across reboots/hotplug; discovery revalidates them
/// by matching against currently reported displays and otherwise marks the
/// source unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DisplayId(pub u32);

/// Session-scoped identifier for a window. Not guaranteed stable across app
/// restarts or window close/reopen; a saved window target that cannot be
/// re-identified must fall back to `SourceAvailability::Unavailable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WindowId(pub u32);

/// Selects the video source for a recording.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VideoSourceSelector {
    /// No video source has been chosen yet.
    #[default]
    None,
    /// A camera device, identified by its persistent unique ID.
    Camera { device_id: DeviceId, label: String },
    /// An entire display.
    Display {
        display_id: DisplayId,
        label: String,
    },
    /// A single window, identified by session-scoped window id plus the
    /// owning application's bundle id (used to help re-identify it).
    Window {
        window_id: WindowId,
        owner_bundle_id: Option<BundleId>,
        title: String,
    },
    /// All windows belonging to an application, captured on a given display.
    Application {
        bundle_id: BundleId,
        display_id: DisplayId,
        label: String,
    },
}

/// Selects the audio source for a recording. Independent of the video
/// selector: e.g. a camera can be paired with application audio.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AudioSourceSelector {
    /// No audio source has been chosen yet. Audio is required; this does not
    /// select a video-only recording mode.
    #[default]
    None,
    /// A microphone / audio input device.
    Microphone { device_id: DeviceId, label: String },
    /// Audio played by a specific application (app-wide, not per-window).
    Application { bundle_id: BundleId, label: String },
    /// All system audio output.
    System,
}

/// Normalized crop rectangle plus canvas placement/scale for the OBS-like
/// single-source viewport editor. All source-space fields are normalized to
/// `[0, 1]` so they remain meaningful across source resolution changes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ViewportTransform {
    /// Crop rectangle in normalized source coordinates (0..1).
    pub crop_x: f64,
    pub crop_y: f64,
    pub crop_width: f64,
    pub crop_height: f64,
    /// Placement of the cropped content within the fixed output canvas,
    /// normalized to the canvas size (0..1).
    pub offset_x: f64,
    pub offset_y: f64,
    pub scale: f64,
}

impl Default for ViewportTransform {
    fn default() -> Self {
        Self {
            crop_x: 0.0,
            crop_y: 0.0,
            crop_width: 1.0,
            crop_height: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
            scale: 1.0,
        }
    }
}

impl ViewportTransform {
    /// Resets crop and placement to show the entire source, scaled to fill
    /// the canvas while preserving aspect ratio (see `render::transform` in
    /// a later step for the actual geometry math).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Returns true if the crop rectangle is within valid normalized bounds.
    pub fn is_crop_valid(&self) -> bool {
        self.crop_x >= 0.0
            && self.crop_y >= 0.0
            && self.crop_width > 0.0
            && self.crop_height > 0.0
            && self.crop_x + self.crop_width <= 1.0 + f64::EPSILON
            && self.crop_y + self.crop_height <= 1.0 + f64::EPSILON
    }
}

/// Supported container/codec pairing. Only one combination is offered in
/// this version, but the setting is modeled explicitly for future growth.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputFormat {
    #[default]
    Mp4H264Aac,
}

/// Resolution in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl Default for Resolution {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
        }
    }
}

/// Output destination and encoding settings for a single recording.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputSettings {
    pub format: OutputFormat,
    pub resolution: Resolution,
    pub fps: u32,
    pub directory: PathBuf,
    /// Filename template, e.g. `{name}_{date}_{time}_{counter}.mp4`.
    pub filename_template: String,
}

impl Default for OutputSettings {
    fn default() -> Self {
        Self {
            format: OutputFormat::default(),
            resolution: Resolution::default(),
            fps: 30,
            directory: default_output_directory(),
            filename_template: "{name}_{date}_{time}_{counter}.mp4".to_string(),
        }
    }
}

/// Best-effort default output directory (`~/Movies/Oculi`), falling back to
/// the home directory, and finally the current directory if neither is
/// resolvable (e.g. in a sandboxed test runner).
pub fn default_output_directory() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let mut path = PathBuf::from(home);
        path.push("Movies");
        path.push("Oculi");
        path
    } else {
        PathBuf::from(".")
    }
}

impl OutputSettings {
    /// Validates resolution/fps bounds and the MP4 filename template. Directory writability is checked
    /// separately by the storage layer, since it requires I/O.
    pub fn validate_format(&self) -> Result<(), String> {
        if self.resolution.width == 0 || self.resolution.height == 0 {
            return Err("Resolution must be non-zero".to_string());
        }
        if !self.resolution.width.is_multiple_of(2) || !self.resolution.height.is_multiple_of(2) {
            return Err("H.264 requires even width and height".to_string());
        }
        if self.fps == 0 || self.fps > 120 {
            return Err("Frame rate must be between 1 and 120".to_string());
        }
        crate::storage::naming::validate_template(&self.filename_template)
    }
}

/// Whether a previously configured source can currently be used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceAvailability {
    /// The source was found during the latest discovery pass.
    Available,
    /// The source could not be found (disconnected, closed window, removed
    /// app, permission revoked, etc). The UI must mark it distinctly and
    /// must not silently substitute another source.
    Unavailable,
    /// Discovery has not completed yet; treat as unknown/pending.
    #[default]
    Unknown,
}

/// A single recording configuration: one video source, one audio source,
/// its own framing and output settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingConfig {
    pub id: RecordingId,
    pub name: String,
    pub video: VideoSourceSelector,
    pub audio: AudioSourceSelector,
    pub viewport: ViewportTransform,
    pub output: OutputSettings,
    #[serde(default)]
    pub video_availability: SourceAvailability,
    #[serde(default)]
    pub audio_availability: SourceAvailability,
}

impl RecordingConfig {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: RecordingId::new(),
            name: name.into(),
            video: VideoSourceSelector::default(),
            audio: AudioSourceSelector::default(),
            viewport: ViewportTransform::default(),
            output: OutputSettings::default(),
            video_availability: SourceAvailability::Unknown,
            audio_availability: SourceAvailability::Unknown,
        }
    }

    /// Duplicates this configuration with a fresh id and a "Copy" suffixed
    /// name, keeping every other setting (video/audio/viewport/output).
    pub fn duplicate(&self) -> Self {
        Self {
            id: RecordingId::new(),
            name: format!("{} Copy", self.name),
            video: self.video.clone(),
            audio: self.audio.clone(),
            viewport: self.viewport,
            output: self.output.clone(),
            video_availability: self.video_availability,
            audio_availability: self.audio_availability,
        }
    }
}

/// Lifecycle state of the whole recording group (shared across every
/// configured recording; controls always operate on the group as a whole).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupState {
    #[default]
    Idle,
    Preparing,
    Recording,
    Paused,
    Finalizing,
    Failed,
}

/// Commands accepted by the group coordinator (see step 5); modeled here so
/// the UI layer can reference a single shared enum from the start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupCommand {
    Start,
    Pause,
    Resume,
    Stop,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_config_duplicate_keeps_settings_but_changes_identity() {
        let mut original = RecordingConfig::new("Camera A");
        original.video = VideoSourceSelector::Camera {
            device_id: DeviceId("cam-1".into()),
            label: "FaceTime HD".into(),
        };
        original.video_availability = SourceAvailability::Available;

        let copy = original.duplicate();

        assert_ne!(copy.id, original.id);
        assert_eq!(copy.name, "Camera A Copy");
        assert_eq!(copy.video, original.video);
        assert_eq!(copy.video_availability, original.video_availability);
    }

    #[test]
    fn viewport_default_is_full_frame_and_valid() {
        let viewport = ViewportTransform::default();
        assert!(viewport.is_crop_valid());
    }

    #[test]
    fn viewport_crop_out_of_bounds_is_invalid() {
        let viewport = ViewportTransform {
            crop_x: 0.5,
            crop_y: 0.0,
            crop_width: 0.7,
            crop_height: 1.0,
            ..Default::default()
        };
        assert!(!viewport.is_crop_valid());
    }

    #[test]
    fn output_settings_rejects_odd_resolution() {
        let output = OutputSettings {
            resolution: Resolution {
                width: 1921,
                height: 1080,
            },
            ..Default::default()
        };
        assert!(output.validate_format().is_err());
    }

    #[test]
    fn output_settings_rejects_zero_fps() {
        let output = OutputSettings {
            fps: 0,
            ..Default::default()
        };
        assert!(output.validate_format().is_err());
    }

    #[test]
    fn output_settings_defaults_are_valid() {
        assert!(OutputSettings::default().validate_format().is_ok());
    }
}
