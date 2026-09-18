//! Worker-local H.264/AAC writing. All timestamps belong to the group's timeline.
//! Create, append, cancel, and drop on the native worker, never the AppKit thread.

use crate::model::{OutputSettings, Resolution};
use crate::platform::macos::sample::{NativeSample, SampleKind};
use crate::render::RenderedFrame;
use block2::RcBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_av_foundation::*;
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, kAudioFormatFlagIsNonInterleaved,
    kAudioFormatLinearPCM, kAudioFormatMPEG4AAC,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::*;
use objc2_core_video::*;
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};
use std::marker::PhantomData;
use std::path::Path;
use std::ptr::{NonNull, null, null_mut};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

// AVAudioSettings.h moved to AVFAudio; AVFoundation's generated module is empty.
#[link(name = "AVFAudio", kind = "framework")]
unsafe extern "C" {
    static AVFormatIDKey: &'static NSString;
    static AVSampleRateKey: &'static NSString;
    static AVNumberOfChannelsKey: &'static NSString;
    static AVEncoderBitRateKey: &'static NSString;
}

struct NativeWriter {
    writer: Retained<AVAssetWriter>,
    video: Retained<AVAssetWriterInput>,
    audio: Retained<AVAssetWriterInput>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
}

pub struct Writer {
    native: Option<NativeWriter>,
    canvas: Resolution,
    last_video: Option<f64>,
    audio_timing: Option<AudioTiming>,
    _local: PhantomData<Rc<()>>,
}

/// Contains no native objects. Poll from the worker with a caller-owned deadline.
/// Dropping this handle does not interrupt native finalization or release its data.
pub struct FinishHandle {
    result: Receiver<Result<(), String>>,
}

impl FinishHandle {
    /// A terminal result is delivered once; `None` means completion is pending.
    pub fn poll(&self) -> Option<Result<(), String>> {
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("Writer completion channel closed".into())),
        }
    }
}

impl Writer {
    /// The staging path must not already exist. Storage reservation is external.
    /// Audio is encoded as 48 kHz stereo AAC, video as SDR/sRGB H.264.
    pub fn prepare(staging_path: &Path, output: &OutputSettings) -> Result<Self, String> {
        output.validate_format()?;
        if output.resolution.width > i32::MAX as u32 || output.resolution.height > i32::MAX as u32 {
            return Err("H.264 dimensions exceed native limits".into());
        }
        let path = staging_path.to_str().ok_or("Staging path is not UTF-8")?;
        if path.is_empty() || !staging_path.is_absolute() {
            return Err("Staging path must be absolute".into());
        }
        autoreleasepool(|_| unsafe {
            let url = NSURL::fileURLWithPath(&NSString::from_str(path));
            let video_type = AVMediaTypeVideo.ok_or("Native video media type unavailable")?;
            let audio_type = AVMediaTypeAudio.ok_or("Native audio media type unavailable")?;
            let writer = AVAssetWriter::assetWriterWithURL_fileType_error(
                &url,
                AVFileTypeMPEG4.ok_or("Native MP4 unavailable")?,
            )
            .map_err(|error| error.to_string())?;
            let video_settings = video_settings(output)?;
            let audio_settings = dictionary(&[
                (AVFormatIDKey, &*NSNumber::new_u32(kAudioFormatMPEG4AAC)),
                (AVSampleRateKey, &*NSNumber::new_f64(48_000.0)),
                (AVNumberOfChannelsKey, &*NSNumber::new_u32(2)),
                (AVEncoderBitRateKey, &*NSNumber::new_u32(128_000)),
            ]);
            if !writer.canApplyOutputSettings_forMediaType(Some(&video_settings), video_type)
                || !writer.canApplyOutputSettings_forMediaType(Some(&audio_settings), audio_type)
            {
                return Err("Native H.264/AAC output settings are unsupported".into());
            }
            let video = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
                video_type,
                Some(&video_settings),
            );
            let audio = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
                audio_type,
                Some(&audio_settings),
            );
            video.setExpectsMediaDataInRealTime(true);
            audio.setExpectsMediaDataInRealTime(true);
            if !writer.canAddInput(&video) || !writer.canAddInput(&audio) {
                return Err("Native writer cannot add H.264/AAC inputs".into());
            }
            writer.addInput(&video);
            writer.addInput(&audio);
            let adaptor = AVAssetWriterInputPixelBufferAdaptor::
                assetWriterInputPixelBufferAdaptorWithAssetWriterInput_sourcePixelBufferAttributes(
                    &video, None,
                );
            let mut result = Self {
                native: Some(NativeWriter {
                    writer,
                    video,
                    audio,
                    adaptor,
                }),
                canvas: output.resolution,
                last_video: None,
                audio_timing: None,
                _local: PhantomData,
            };
            let native = result.native.as_ref().unwrap();
            if !native.writer.startWriting() {
                let error = native_error(&native.writer, "Cannot start writing");
                result.cancel();
                return Err(error);
            }
            native.writer.startSessionAtSourceTime(kCMTimeZero);
            Ok(result)
        })
    }

    /// `false` explicitly drops this video frame for backpressure. No timestamp
    /// is committed on false; the caller may retry or count the dropped frame.
    pub fn append_video(&mut self, frame: &RenderedFrame, timestamp: f64) -> Result<bool, String> {
        let time = media_time(timestamp)?;
        if frame.canvas != self.canvas {
            return Err("Rendered frame dimensions do not match output".into());
        }
        if self.last_video.is_some_and(|last| timestamp <= last) {
            return Err("Video timestamps must increase strictly".into());
        }
        let native = self.writing()?;
        let accepted = autoreleasepool(|_| unsafe {
            if !native.video.isReadyForMoreMediaData() {
                return readiness(&native.writer, false);
            }
            // The frame remains alive for the entire synchronous read. A private
            // copy avoids retaining pooled RenderedFrames for an unbounded encode
            // lifetime: AVFoundation retains only our copy until consumption.
            let pixels = copy_video(frame)?;
            readiness(
                &native.writer,
                native
                    .adaptor
                    .appendPixelBuffer_withPresentationTime(&pixels, time),
            )
        })?;
        if accepted {
            self.last_video = Some(timestamp);
        }
        Ok(accepted)
    }

    /// Copies only the selected PCM frames and assigns their group-mapped PTS.
    /// `false` is backpressure, NEVER permission to silently discard audio: the
    /// caller must retain/retry the same span with a bounded queue/deadline or fail.
    pub fn append_audio(
        &mut self,
        sample: &NativeSample,
        offset: usize,
        frames: usize,
        timestamp: f64,
    ) -> Result<bool, String> {
        let time = media_time(timestamp)?;
        let native = self.writing()?;
        let (accepted, timing) = autoreleasepool(|_| unsafe {
            // Validate even when the input is not ready, without native mutation.
            let asbd = pcm_description(sample, offset, frames)?;
            let duration = frames as f64 / asbd.mSampleRate;
            let timing =
                AudioTiming::next(self.audio_timing, timestamp, duration, asbd.mSampleRate)?;
            if !native.audio.isReadyForMoreMediaData() {
                return Ok((readiness(&native.writer, false)?, timing));
            }
            let copied = copy_audio(sample, offset, frames, time, asbd)?;
            Ok::<_, String>((
                readiness(&native.writer, native.audio.appendSampleBuffer(&copied))?,
                timing,
            ))
        })?;
        if accepted {
            self.audio_timing = Some(timing);
        }
        Ok(accepted)
    }

    /// Ends at the common active duration, not at an independently rebased track
    /// end. Returns immediately; caller bounds polling time before publication.
    pub fn finish(&mut self, end: f64) -> Result<FinishHandle, String> {
        let end_time = media_time(end)?;
        if self.last_video.is_some_and(|last| end <= last) {
            return Err("Session end must follow the final video timestamp".into());
        }
        self.writing()?;
        let native = self.native.take().unwrap();
        let (sender, result) = mpsc::channel();
        unsafe {
            native.writer.endSessionAtSourceTime(end_time);
            native.video.markAsFinished();
            native.audio.markAsFinished();
            let writer = native.writer.clone();
            // SAFETY: AVFoundation invokes this block once after all appends
            // finish. Exclusive ownership transfers into this block; no worker
            // alias remains. Only documented completion-time status/error reads
            // and native releases run here. No blanket native Send impl, no
            // AppKit access, and only an owned Rust result crosses the channel.
            let completion = RcBlock::new(move || {
                autoreleasepool(|_| {
                    let result = if native.writer.status() == AVAssetWriterStatus::Completed {
                        Ok(())
                    } else {
                        Err(native_error(
                            &native.writer,
                            "Native MP4 finalization failed",
                        ))
                    };
                    let _ = sender.send(result);
                    // Capture the complete native owner through completion.
                    let _ = (&native.video, &native.audio, &native.adaptor);
                });
            });
            writer.finishWritingWithCompletionHandler(&completion);
        }
        Ok(FinishHandle { result })
    }

    /// Worker-only rollback. Native cancellation can wait for an in-flight encode.
    /// After finish, ownership belongs to the completion block and this is a no-op.
    pub fn cancel(&mut self) {
        if let Some(native) = self.native.take() {
            unsafe { native.writer.cancelWriting() };
        }
    }

    fn writing(&self) -> Result<&NativeWriter, String> {
        let native = self
            .native
            .as_ref()
            .ok_or("Writer is finished or cancelled")?;
        unsafe {
            readiness(&native.writer, true)?;
        }
        Ok(native)
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.cancel();
    }
}

const AUDIO_CLOCK_DRIFT: f64 = 0.001;
const AUDIO_TIMESTAMP_JITTER: f64 = 0.001;

#[derive(Clone, Copy, Debug, PartialEq)]
struct AudioTiming {
    start: f64,
    end: f64,
    minimum_next: f64,
}

impl AudioTiming {
    // Propose timing without committing it: refused appends must be retryable.
    fn next(
        previous: Option<Self>,
        timestamp: f64,
        duration: f64,
        rate: f64,
    ) -> Result<Self, String> {
        let mut minimum_start = timestamp;
        if let Some(previous) = previous {
            // Device PTS can fluctuate more than the clock's long-term rate.
            // Bound phase jitter to 1 ms, and to 10% of short/trimmed buffers.
            let jitter = AUDIO_TIMESTAMP_JITTER.min((previous.end - previous.start) * 0.1);
            validate_audio_timestamp(timestamp, previous.start, previous.end, rate, jitter)?;
            // A per-buffer allowance alone would admit sustained overlapping
            // PCM. Carry a lower clock envelope forward at the fastest supported
            // rate; jitter and rounding get one shared budget, not one per buffer.
            let tolerance = AUDIO_TIMESTAMP_JITTER + 1.0 / rate;
            if previous.minimum_next - timestamp > tolerance {
                return Err(format!(
                    "Audio spans accumulate overlap beyond clock jitter: earliest next \
                     {:.9}s, next {timestamp:.9}s (jitter tolerance {tolerance:.9}s)",
                    previous.minimum_next
                ));
            }
            minimum_start = minimum_start.max(previous.minimum_next);
        }
        Ok(Self {
            start: timestamp,
            end: timestamp + duration,
            minimum_next: minimum_start + duration * (1.0 - AUDIO_CLOCK_DRIFT),
        })
    }
}

fn validate_audio_timestamp(
    timestamp: f64,
    previous: f64,
    end: f64,
    rate: f64,
    jitter: f64,
) -> Result<(), String> {
    // Host-converted PTS includes hardware drift; PCM duration is nominal.
    // Allow one PCM frame of quantization plus the 0.1% clock-rate tolerance
    // used by capture and bounded phase jitter. Keep the original group PTS for
    // AVFoundation instead of snapping to nominal ends (accumulating A/V drift).
    let tolerance = 1.0 / rate + (end - previous) * AUDIO_CLOCK_DRIFT + jitter;
    if timestamp <= previous || end - timestamp > tolerance {
        return Err(format!(
            "Audio spans must not overlap or run backwards: previous start {previous:.9}s, \
             end {end:.9}s, next {timestamp:.9}s (clock tolerance {tolerance:.9}s)"
        ));
    }
    Ok(())
}

fn media_time(seconds: f64) -> Result<CMTime, String> {
    if !seconds.is_finite() || !(0.0..1e9).contains(&seconds) {
        return Err("Media time must be finite, nonnegative, and representable".into());
    }
    Ok(CMTime {
        value: (seconds * 1_000_000_000.0).round() as i64,
        timescale: 1_000_000_000,
        flags: CMTimeFlags::Valid,
        epoch: 0,
    })
}

unsafe fn native_error(writer: &AVAssetWriter, context: &str) -> String {
    unsafe {
        format!(
            "{context}: {}",
            writer
                .error()
                .map(|error| error.to_string())
                .unwrap_or_else(|| format!("native status {}", writer.status().0))
        )
    }
}

unsafe fn readiness(writer: &AVAssetWriter, accepted: bool) -> Result<bool, String> {
    unsafe {
        if writer.status() == AVAssetWriterStatus::Writing {
            Ok(accepted)
        } else {
            Err(native_error(writer, "Writer is not writing"))
        }
    }
}

fn dictionary(entries: &[(&NSString, &AnyObject)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    NSDictionary::from_slices(
        &entries.iter().map(|entry| entry.0).collect::<Vec<_>>(),
        &entries.iter().map(|entry| entry.1).collect::<Vec<_>>(),
    )
}

unsafe fn video_settings(
    output: &OutputSettings,
) -> Result<Retained<NSDictionary<NSString, AnyObject>>, String> {
    unsafe {
        let key = |value: Option<&'static NSString>| {
            value.ok_or_else(|| "Native video settings unavailable".to_string())
        };
        let color = dictionary(&[
            (
                key(AVVideoColorPrimariesKey)?,
                key(AVVideoColorPrimaries_ITU_R_709_2)?,
            ),
            (
                key(AVVideoTransferFunctionKey)?,
                key(AVVideoTransferFunction_IEC_sRGB)?,
            ),
            (
                key(AVVideoYCbCrMatrixKey)?,
                key(AVVideoYCbCrMatrix_ITU_R_709_2)?,
            ),
        ]);
        let compression = dictionary(&[
            (
                key(AVVideoExpectedSourceFrameRateKey)?,
                &*NSNumber::new_u32(output.fps),
            ),
            (
                key(AVVideoAllowFrameReorderingKey)?,
                &*NSNumber::new_bool(false),
            ),
        ]);
        Ok(dictionary(&[
            (key(AVVideoCodecKey)?, key(AVVideoCodecTypeH264)?),
            (
                key(AVVideoWidthKey)?,
                &*NSNumber::new_u32(output.resolution.width),
            ),
            (
                key(AVVideoHeightKey)?,
                &*NSNumber::new_u32(output.resolution.height),
            ),
            (key(AVVideoColorPropertiesKey)?, &*color),
            (key(AVVideoCompressionPropertiesKey)?, &*compression),
        ]))
    }
}

fn os_status(status: i32, operation: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("{operation}: native status {status}"))
    }
}

unsafe fn copy_video(frame: &RenderedFrame) -> Result<CFRetained<CVPixelBuffer>, String> {
    unsafe {
        let source = frame.pixel_buffer_read();
        let width = CVPixelBufferGetWidth(source);
        let height = CVPixelBufferGetHeight(source);
        if CVPixelBufferGetPixelFormatType(source) != kCVPixelFormatType_32BGRA
            || width != frame.canvas.width as usize
            || height != frame.canvas.height as usize
        {
            return Err("Rendered frame is not the expected BGRA canvas".into());
        }
        let mut raw = null_mut();
        os_status(
            CVPixelBufferCreate(
                None,
                width,
                height,
                kCVPixelFormatType_32BGRA,
                None,
                NonNull::from(&mut raw),
            ),
            "Allocate writer pixels",
        )?;
        let destination = CFRetained::from_raw(NonNull::new(raw).ok_or("Missing writer pixels")?);
        os_status(
            CVPixelBufferLockBaseAddress(source, CVPixelBufferLockFlags::ReadOnly),
            "Lock rendered pixels",
        )?;
        let result = (|| {
            os_status(
                CVPixelBufferLockBaseAddress(&destination, CVPixelBufferLockFlags::empty()),
                "Lock writer pixels",
            )?;
            let src = CVPixelBufferGetBaseAddress(source).cast::<u8>();
            let dst = CVPixelBufferGetBaseAddress(&destination).cast::<u8>();
            let result: Result<(), String> = if src.is_null() || dst.is_null() {
                Err("Missing BGRA storage".into())
            } else {
                for row in 0..height {
                    std::ptr::copy_nonoverlapping(
                        src.add(row * CVPixelBufferGetBytesPerRow(source)),
                        dst.add(row * CVPixelBufferGetBytesPerRow(&destination)),
                        width * 4,
                    );
                }
                Ok(())
            };
            CVPixelBufferUnlockBaseAddress(&destination, CVPixelBufferLockFlags::empty());
            result
        })();
        CVPixelBufferUnlockBaseAddress(source, CVPixelBufferLockFlags::ReadOnly);
        result?;
        destination.set_attachment(
            kCVImageBufferColorPrimariesKey,
            kCVImageBufferColorPrimaries_ITU_R_709_2,
            CVAttachmentMode::ShouldPropagate,
        );
        destination.set_attachment(
            kCVImageBufferTransferFunctionKey,
            kCVImageBufferTransferFunction_sRGB,
            CVAttachmentMode::ShouldPropagate,
        );
        destination.set_attachment(
            kCVImageBufferYCbCrMatrixKey,
            kCVImageBufferYCbCrMatrix_ITU_R_709_2,
            CVAttachmentMode::ShouldPropagate,
        );
        Ok(destination)
    }
}

unsafe fn pcm_description(
    sample: &NativeSample,
    offset: usize,
    frames: usize,
) -> Result<AudioStreamBasicDescription, String> {
    unsafe {
        if sample.kind() != SampleKind::Audio
            || frames == 0
            || frames > i32::MAX as usize
            || offset > i32::MAX as usize
        {
            return Err("Expected a nonempty PCM audio range".into());
        }
        let buffer = sample.buffer_read();
        if !buffer.is_valid()
            || !buffer.data_is_ready()
            || offset
                .checked_add(frames)
                .is_none_or(|end| end > buffer.num_samples() as usize)
        {
            return Err("PCM range exceeds the ready sample".into());
        }
        let format = buffer.format_description().ok_or("Missing audio format")?;
        let asbd = *CMAudioFormatDescriptionGetStreamBasicDescription(&format)
            .as_ref()
            .ok_or("Missing PCM description")?;
        if asbd.mFormatID != kAudioFormatLinearPCM
            || !asbd.mSampleRate.is_finite()
            || !(8_000.0..=192_000.0).contains(&asbd.mSampleRate)
            || asbd.mSampleRate.fract() != 0.0
            || !(1..=2).contains(&asbd.mChannelsPerFrame)
            || asbd.mBytesPerFrame == 0
            || asbd.mBytesPerFrame > 16
            || asbd.mFramesPerPacket != 1
        {
            return Err("Unsupported PCM format (requires mono/stereo, 8–192 kHz)".into());
        }
        Ok(asbd)
    }
}

// Core Audio's trailing array has one entry in its binding. This correctly
// aligned, repr(C) storage provides room for the two supported planar channels.
#[repr(C)]
struct PcmBuffers {
    count: u32,
    buffers: [AudioBuffer; 2],
}

unsafe fn copy_audio(
    sample: &NativeSample,
    offset: usize,
    frames: usize,
    timestamp: CMTime,
    mut asbd: AudioStreamBasicDescription,
) -> Result<CFRetained<CMSampleBuffer>, String> {
    unsafe {
        let planar = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
        let count = if planar { asbd.mChannelsPerFrame } else { 1 };
        let bytes = frames
            .checked_mul(asbd.mBytesPerFrame as usize)
            .ok_or("PCM size overflow")?;
        // Bounded allocation even for a malformed or unexpectedly huge callback.
        if bytes > 16 * 1024 * 1024 {
            return Err("PCM span exceeds 16 MiB per plane".into());
        }
        let mut data = [
            vec![0u8; bytes],
            if count == 2 {
                vec![0u8; bytes]
            } else {
                Vec::new()
            },
        ];
        let mut list = PcmBuffers {
            count,
            buffers: std::array::from_fn(|index| AudioBuffer {
                mNumberChannels: if planar { 1 } else { asbd.mChannelsPerFrame },
                mDataByteSize: data[index].len() as u32,
                mData: data[index].as_mut_ptr().cast(),
            }),
        };
        let list = NonNull::from(&mut list).cast::<AudioBufferList>();
        os_status(
            sample.buffer_read().copy_pcm_data_into_audio_buffer_list(
                offset as i32,
                frames as i32,
                list,
            ),
            "Copy PCM range",
        )?;
        // Recreate the format too: no native alias of NativeSample escapes.
        let mut raw_format = null();
        os_status(
            CMAudioFormatDescriptionCreate(
                None,
                NonNull::from(&mut asbd),
                0,
                null(),
                0,
                null(),
                None,
                NonNull::from(&mut raw_format),
            ),
            "Copy PCM format",
        )?;
        let format =
            CFRetained::from_raw(NonNull::new(raw_format.cast_mut()).ok_or("Missing PCM format")?);
        let timing = CMSampleTimingInfo {
            duration: CMTime {
                value: 1,
                timescale: asbd.mSampleRate as i32,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            },
            presentationTimeStamp: timestamp,
            decodeTimeStamp: kCMTimeInvalid,
        };
        let mut raw = null_mut();
        let size = asbd.mBytesPerFrame as usize;
        os_status(
            CMSampleBuffer::create_ready(
                None,
                None,
                Some(&format),
                frames as isize,
                1,
                &timing,
                if planar { 0 } else { 1 },
                if planar { null() } else { &size },
                NonNull::from(&mut raw),
            ),
            "Create retimed PCM",
        )?;
        let copied = CFRetained::from_raw(NonNull::new(raw).ok_or("Missing retimed PCM")?);
        // This call copies data into CM-owned memory, never Rust Vec storage.
        os_status(
            copied.set_data_buffer_from_audio_buffer_list(None, None, 0, list),
            "Own retimed PCM storage",
        )?;
        Ok(copied)
    }
}

#[cfg(test)]
#[path = "writer_tests.rs"]
mod tests;
