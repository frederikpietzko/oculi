//! Fixed-canvas rendering shared by preview and the future writer.

pub mod transform;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{PreviewPresenter, RenderedFrame, Renderer, synthetic_preview_frame};

#[cfg(target_os = "macos")]
pub(crate) use macos::synthetic_sample;
