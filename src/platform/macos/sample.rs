//! Immutable captured media. Native reads require an explicit unsafe contract.

use crate::model::Resolution;
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMSampleBuffer, CMTime, CMTimeFlags, kCMMediaType_Audio, kCMMediaType_Video,
};
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth};
use std::ptr::NonNull;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleKind {
    Video,
    Audio,
}

#[derive(Clone)]
pub struct NativeSample {
    // Keeping the sample alive also keeps its image/audio storage alive. Never
    // expose even &CMSampleBuffer: many Core Media mutators take shared refs.
    _buffer: CFRetained<CMSampleBuffer>,
    kind: SampleKind,
    timestamp: f64,
    dimensions: Option<Resolution>,
}

// SAFETY: This is deliberately NOT an implementation for CMSampleBuffer or a
// generic Objective-C/CF holder. The unsafe constructor requires a completed,
// immutable capture sample, with no future invalidation, mutation, or borrowed
// backing storage, including attachments and custom disposal callbacks. CF
// retain/release may run on any thread. Cloning only retains; all public reads
// use copied Rust metadata, and there is no safe access to the native object.
// Holding the sample prevents AVFoundation/ScreenCaptureKit from recycling its
// backing storage until the final release. Future accessors must preserve this
// invariant; the crate-only unsafe read accessor transfers these obligations to
// its caller, never granting safe native mutation or untracked buffer escape.
unsafe impl Send for NativeSample {}
// SAFETY: Sharing only exposes immutable copied metadata and thread-safe CF
// reference counting, under the same constructor obligations as Send above.
unsafe impl Sync for NativeSample {}

impl NativeSample {
    /// Retains a sample before returning from a native capture callback.
    ///
    /// # Safety
    /// The buffer and its complete object graph must remain immutable until the
    /// last clone is dropped. It must own its backing storage; any custom memory
    /// allocator/disposal callback must support release on arbitrary threads.
    /// No alias may invalidate the sample or mutate its data/attachments. Ready
    /// samples delivered by AVFoundation and ScreenCaptureKit meet this contract
    /// provided the application does not mutate them. `kind` must describe the
    /// callback's media type (also checked against the format description).
    pub unsafe fn retain(buffer: &CMSampleBuffer, kind: SampleKind) -> Option<Self> {
        // SAFETY: Caller guarantees a live, completed, immutable capture sample.
        unsafe {
            if !buffer.is_valid() || !buffer.data_is_ready() || buffer.num_samples() <= 0 {
                return None;
            }
            let format = buffer.format_description()?;
            let expected = match kind {
                SampleKind::Video => kCMMediaType_Video,
                SampleKind::Audio => kCMMediaType_Audio,
            };
            if format.media_type() != expected {
                return None;
            }
            let timestamp = numeric_seconds(buffer.presentation_time_stamp())?;
            let dimensions = buffer.image_buffer().and_then(|image| {
                Some(Resolution {
                    width: u32::try_from(CVPixelBufferGetWidth(&image)).ok()?,
                    height: u32::try_from(CVPixelBufferGetHeight(&image)).ok()?,
                })
            });
            Some(Self {
                _buffer: CFRetained::retain(NonNull::from(buffer)),
                kind,
                timestamp,
                dimensions,
            })
        }
    }

    pub fn kind(&self) -> SampleKind {
        self.kind
    }

    pub fn timestamp_seconds(&self) -> f64 {
        self.timestamp
    }

    /// Stores host-clock seconds for production capture metadata. The immutable
    /// native PTS is unchanged; the writer separately copies and retimes PCM.
    pub(crate) fn with_host_timestamp(mut self, seconds: f64) -> Result<Self, String> {
        if !seconds.is_finite() || seconds < 0.0 {
            return Err("Host timestamp must be finite and nonnegative".into());
        }
        self.timestamp = seconds;
        Ok(self)
    }

    pub fn dimensions(&self) -> Option<Resolution> {
        self.dimensions
    }

    /// Native image access exclusively for immutable rendering.
    ///
    /// # Safety
    /// Do not mutate the buffer, attachments, planes, or any backing storage.
    /// Do not expose it through safe APIs or let native aliases escape the
    /// read operation. Retain this sample until all asynchronous GPU reads
    /// complete; CIImage/texture aliases must be released before that owner.
    pub(crate) unsafe fn image_buffer_read(&self) -> Option<CFRetained<CVPixelBuffer>> {
        unsafe { self._buffer.image_buffer() }
    }

    /// Native access exclusively for synchronous PCM copying by the writer.
    ///
    /// # Safety
    /// Never mutate or invalidate the sample, attachments, format, or storage.
    /// Keep this owner alive throughout the read. Neither the reference nor
    /// retained native aliases may escape the read operation; asynchronous
    /// consumers must receive independently owned copies, including the format.
    pub(crate) unsafe fn buffer_read(&self) -> &CMSampleBuffer {
        &self._buffer
    }
}

fn numeric_seconds(time: CMTime) -> Option<f64> {
    if !time.flags.contains(CMTimeFlags::Valid)
        || time.flags.intersects(CMTimeFlags::ImpliedValueFlagsMask)
        || time.timescale <= 0
    {
        return None;
    }
    Some(time.value as f64 / f64::from(time.timescale))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_rejects_non_numeric_times() {
        let mut time = CMTime {
            value: 96_000,
            timescale: 48_000,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        assert_eq!(numeric_seconds(time), Some(2.0));
        for flags in [
            CMTimeFlags::empty(),
            CMTimeFlags::Valid | CMTimeFlags::Indefinite,
            CMTimeFlags::Valid | CMTimeFlags::PositiveInfinity,
            CMTimeFlags::Valid | CMTimeFlags::NegativeInfinity,
        ] {
            time.flags = flags;
            assert_eq!(numeric_seconds(time), None);
        }
        time.flags = CMTimeFlags::Valid;
        time.timescale = 0;
        assert_eq!(numeric_seconds(time), None);
    }

    #[test]
    fn concrete_sample_is_send_sync_clone() {
        fn assert_traits<T: Send + Sync + Clone>() {}
        assert_traits::<NativeSample>();
    }

    #[test]
    fn rejects_unready_invalid_and_marker_buffers() {
        // SAFETY: These are private empty test buffers, with no callbacks or
        // borrowed backing memory. No NativeSample is produced before mutation.
        unsafe {
            let mut raw = std::ptr::null_mut();
            assert_eq!(
                CMSampleBuffer::create(
                    None,
                    None,
                    false,
                    None,
                    std::ptr::null_mut(),
                    None,
                    0,
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    NonNull::from(&mut raw)
                ),
                0
            );
            let buffer = CFRetained::from_raw(NonNull::new(raw).unwrap());
            assert!(!buffer.data_is_ready());
            assert!(NativeSample::retain(&buffer, SampleKind::Audio).is_none());
            assert_eq!(buffer.set_data_ready(), 0);
            assert!(NativeSample::retain(&buffer, SampleKind::Video).is_none());
            assert_eq!(buffer.invalidate(), 0);
            assert!(!buffer.is_valid());
            assert!(NativeSample::retain(&buffer, SampleKind::Audio).is_none());
        }
    }
}
