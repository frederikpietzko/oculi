//! Shared capture contracts. Native sessions are owned by a dedicated worker.

pub mod registry;
pub mod service;

use crate::model::{AudioSourceSelector, VideoSourceSelector};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SourceKey {
    Camera(String),
    Microphone(String),
    Display(u32),
    Window { id: u32, owner: Option<String> },
    ApplicationVideo { bundle: String, display: u32 },
    ApplicationAudio(String),
    SystemAudio,
}

impl SourceKey {
    pub fn video(source: &VideoSourceSelector) -> Option<Self> {
        Some(match source {
            VideoSourceSelector::None => return None,
            VideoSourceSelector::Camera { device_id, .. } => Self::Camera(device_id.0.clone()),
            VideoSourceSelector::Display { display_id, .. } => Self::Display(display_id.0),
            VideoSourceSelector::Window {
                window_id,
                owner_bundle_id,
                ..
            } => Self::Window {
                id: window_id.0,
                owner: owner_bundle_id.as_ref().map(|id| id.0.clone()),
            },
            VideoSourceSelector::Application {
                bundle_id,
                display_id,
                ..
            } => Self::ApplicationVideo {
                bundle: bundle_id.0.clone(),
                display: display_id.0,
            },
        })
    }

    pub fn audio(source: &AudioSourceSelector) -> Option<Self> {
        Some(match source {
            AudioSourceSelector::None => return None,
            AudioSourceSelector::Microphone { device_id, .. } => {
                Self::Microphone(device_id.0.clone())
            }
            AudioSourceSelector::Application { bundle_id, .. } => {
                Self::ApplicationAudio(bundle_id.0.clone())
            }
            AudioSourceSelector::System => Self::SystemAudio,
        })
    }

    pub fn is_audio(&self) -> bool {
        matches!(
            self,
            Self::Microphone(_) | Self::ApplicationAudio(_) | Self::SystemAudio
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureFormat {
    Video { width: u32, height: u32, fps: u32 },
    Audio { sample_rate: u32, channels: u16 },
}

impl CaptureFormat {
    pub fn validate(self, source: &SourceKey) -> Result<(), CaptureError> {
        let valid = match self {
            Self::Video { width, height, fps } => {
                !source.is_audio()
                    && width > 0
                    && height > 0
                    && width <= 7680
                    && height <= 4320
                    && (1..=120).contains(&fps)
            }
            Self::Audio {
                sample_rate,
                channels,
            } => source.is_audio() && sample_rate == 48_000 && (1..=2).contains(&channels),
        };
        if valid {
            Ok(())
        } else {
            Err(CaptureError::UnsupportedFormat(
                "Unsupported capture format or source/media mismatch".into(),
            ))
        }
    }

    /// A larger video stream can feed smaller outputs; audio must match exactly.
    pub fn supports(self, requested: Self) -> bool {
        match (self, requested) {
            (
                Self::Video { width, height, fps },
                Self::Video {
                    width: w,
                    height: h,
                    fps: f,
                },
            ) => width >= w && height >= h && fps >= f,
            (a, b) => a == b,
        }
    }

    pub fn merge(self, other: Self) -> Result<Self, CaptureError> {
        match (self, other) {
            (
                Self::Video { width, height, fps },
                Self::Video {
                    width: w,
                    height: h,
                    fps: f,
                },
            ) => Ok(Self::Video {
                width: width.max(w),
                height: height.max(h),
                fps: fps.max(f),
            }),
            (a, b) if a == b => Ok(a),
            _ => Err(CaptureError::UnsupportedFormat(
                "Shared audio formats must match".into(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureRequest {
    pub source: SourceKey,
    pub format: CaptureFormat,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    #[error("Permission required: {0}")]
    Permission(String),
    #[error("Source unavailable: {0}")]
    Unavailable(String),
    #[error("Unsupported format: {0}")]
    UnsupportedFormat(String),
    #[error("Capture failed: {0}")]
    Native(String),
    #[error("Capture timed out: {0}")]
    Timeout(String),
    #[error("Capture overload: {0}")]
    Overload(String),
}

/// Implementations are thread-confined; stop must detach delegates and quiesce callbacks.
pub trait CaptureSession {
    fn stop(&mut self) -> Result<(), CaptureError>;
}

pub trait CaptureBackend<T: Clone + Send + 'static> {
    fn start(
        &mut self,
        request: &CaptureRequest,
        publisher: registry::SamplePublisher<T>,
    ) -> Result<Box<dyn CaptureSession>, CaptureError>;
}
