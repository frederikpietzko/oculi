//! AVFoundation device capture. Create, configure, start, stop, and drop sessions
//! on the capture registry's worker, never on the main or sample callback queue.

use super::permissions;
use super::sample::{NativeSample, SampleKind};
use crate::capture::registry::SamplePublisher;
use crate::capture::{CaptureError, CaptureFormat, CaptureRequest, CaptureSession, SourceKey};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, define_class, msg_send};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureAudioDataOutput, AVCaptureAudioDataOutputSampleBufferDelegate,
    AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceFormat, AVCaptureDeviceInput,
    AVCaptureDeviceWasDisconnectedNotification, AVCaptureOutput, AVCaptureSession,
    AVCaptureSessionDidStopRunningNotification, AVCaptureSessionErrorKey,
    AVCaptureSessionPresetInputPriority, AVCaptureSessionRuntimeErrorNotification,
    AVCaptureSessionWasInterruptedNotification, AVCaptureVideoDataOutput,
    AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeAudio, AVMediaTypeVideo,
    AVVideoScalingModeKey, AVVideoScalingModeResizeAspect,
};
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMFormatDescription, CMSampleBuffer, CMTime,
    CMTimeFlags, CMVideoFormatDescriptionGetDimensions, kCMMediaType_Audio, kCMMediaType_Video,
};
use objc2_core_video::{
    kCVPixelBufferHeightKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{
    NSDictionary, NSError, NSNotification, NSNotificationCenter, NSNumber, NSObject,
    NSObjectProtocol, NSString,
};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::Duration;

// AVAudioSettings is empty in the pinned AVFoundation bindings (the definitions
// moved to AVFAudio). Use the exported constants, not guessed dictionary strings.
#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {
    static AVFormatIDKey: &'static NSString;
    static AVSampleRateKey: &'static NSString;
    static AVNumberOfChannelsKey: &'static NSString;
    static AVLinearPCMBitDepthKey: &'static NSString;
    static AVLinearPCMIsFloatKey: &'static NSString;
    static AVLinearPCMIsBigEndianKey: &'static NSString;
    static AVLinearPCMIsNonInterleaved: &'static NSString;
}

const LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");

trait SampleSink: Send + Sync {
    fn publish(&self, sample: NativeSample);
    fn fail(&self, error: CaptureError);
    fn note_drop(&self);
}

impl SampleSink for SamplePublisher<NativeSample> {
    fn publish(&self, sample: NativeSample) {
        SamplePublisher::publish(self, sample);
    }

    fn fail(&self, error: CaptureError) {
        SamplePublisher::fail(self, error);
    }

    fn note_drop(&self) {
        SamplePublisher::note_drop(self);
    }
}

#[derive(Default)]
struct CallbackStatus {
    stopped: bool,
    error: Option<CaptureError>,
}

struct CallbackState {
    sink: Box<dyn SampleSink>,
    format: CaptureFormat,
    // The same gate covers notifications (arbitrary posting threads) and serial
    // sample callbacks. Closing it waits out publication already in progress.
    status: Mutex<CallbackStatus>,
}

impl CallbackState {
    fn status(&self) -> MutexGuard<'_, CallbackStatus> {
        self.status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn fail(&self, error: CaptureError) {
        let mut status = self.status();
        if !status.stopped && status.error.is_none() {
            status.error = Some(error.clone());
            self.sink.fail(error);
        }
    }

    fn receive(&self, buffer: &CMSampleBuffer, session: &AVCaptureSession) {
        let mut status = self.status();
        if status.stopped || status.error.is_some() {
            return;
        }
        // SAFETY: AVFoundation lends a live, immutable buffer for this callback.
        // No mutation or buffer pointer escapes; NativeSample retains ownership.
        let sample = unsafe {
            validate_sample(buffer, self.format).and_then(|kind| {
                let clock = session.synchronizationClock().ok_or_else(|| {
                    CaptureError::Native("Device synchronization clock is unavailable".into())
                })?;
                let host =
                    crate::recording::clock::to_host(buffer.presentation_time_stamp(), &clock)
                        .map_err(CaptureError::Native)?;
                NativeSample::retain(buffer, kind)
                    .ok_or_else(|| CaptureError::Native("Could not retain device sample".into()))?
                    .with_host_timestamp(host)
                    .map_err(CaptureError::Native)
            })
        };
        match sample {
            Ok(sample) => self.sink.publish(sample),
            Err(error) => {
                status.error = Some(error.clone());
                self.sink.fail(error);
            }
        }
    }

    fn dropped(&self) {
        let status = self.status();
        if !status.stopped && status.error.is_none() {
            self.sink.note_drop();
        }
    }

    fn callback(&self, action: impl FnOnce()) {
        if catch_unwind(AssertUnwindSafe(|| autoreleasepool(|_| action()))).is_err() {
            self.fail(CaptureError::Native(
                "Device capture callback panicked".into(),
            ));
        }
    }
}

struct DeviceDelegateState {
    callbacks: Arc<CallbackState>,
    // Read only synchronizationClock during callbacks. Output delegates are
    // detached and drained during stop before releasing the session graph.
    session: Retained<AVCaptureSession>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "OculiDeviceCaptureDelegate"]
    #[ivars = DeviceDelegateState]
    struct DeviceDelegate;

    unsafe impl NSObjectProtocol for DeviceDelegate {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for DeviceDelegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        unsafe fn did_output(
            &self,
            _output: &AVCaptureOutput,
            buffer: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            let state = self.ivars();
            state
                .callbacks
                .callback(|| state.callbacks.receive(buffer, &state.session));
        }

        #[unsafe(method(captureOutput:didDropSampleBuffer:fromConnection:))]
        unsafe fn did_drop(
            &self,
            _output: &AVCaptureOutput,
            _buffer: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            self.ivars()
                .callbacks
                .callback(|| self.ivars().callbacks.dropped());
        }
    }

    // Both protocols use the identical didOutput selector implemented above.
    unsafe impl AVCaptureAudioDataOutputSampleBufferDelegate for DeviceDelegate {}
);

impl DeviceDelegate {
    fn new(callbacks: Arc<CallbackState>, session: Retained<AVCaptureSession>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DeviceDelegateState { callbacks, session });
        // SAFETY: NSObject initialization, with Rust ivars already initialized.
        unsafe { msg_send![super(this), init] }
    }
}

fn numeric_time(time: CMTime) -> bool {
    time.timescale > 0
        && time.flags.contains(CMTimeFlags::Valid)
        && !time.flags.intersects(CMTimeFlags::ImpliedValueFlagsMask)
}

fn matches_video_output(
    width: i32,
    height: i32,
    pixel_format: u32,
    requested: CaptureFormat,
) -> bool {
    matches!(requested, CaptureFormat::Video { width: w, height: h, .. }
        if width > 0 && height > 0 && width as u32 == w && height as u32 == h
            && pixel_format == kCVPixelFormatType_32BGRA)
}

unsafe fn validate_sample(
    buffer: &CMSampleBuffer,
    requested: CaptureFormat,
) -> Result<SampleKind, CaptureError> {
    // SAFETY: All reads are confined to the live AVFoundation callback buffer.
    unsafe {
        if !buffer.is_valid() || !buffer.data_is_ready() || buffer.num_samples() <= 0 {
            return Err(CaptureError::Native(
                "Device delivered an empty or unready sample".into(),
            ));
        }
        if !numeric_time(buffer.presentation_time_stamp()) {
            return Err(CaptureError::Native(
                "Device sample has no numeric presentation timestamp".into(),
            ));
        }
        let description = buffer.format_description().ok_or_else(|| {
            CaptureError::Native("Device sample has no format description".into())
        })?;
        match requested {
            CaptureFormat::Video { width, height, .. } => {
                if description.media_type() != kCMMediaType_Video {
                    return Err(CaptureError::Native(
                        "Camera delivered non-video media".into(),
                    ));
                }
                let dimensions = CMVideoFormatDescriptionGetDimensions(&description);
                if !matches_video_output(
                    dimensions.width,
                    dimensions.height,
                    description.media_sub_type(),
                    requested,
                ) || buffer.image_buffer().is_none()
                {
                    return Err(CaptureError::UnsupportedFormat(format!(
                        "Camera output conversion did not deliver BGRA {width}x{height}; received {}x{} (pixel format {:#x})",
                        dimensions.width,
                        dimensions.height,
                        description.media_sub_type()
                    )));
                }
                Ok(SampleKind::Video)
            }
            CaptureFormat::Audio {
                sample_rate,
                channels,
            } => {
                if description.media_type() != kCMMediaType_Audio {
                    return Err(CaptureError::Native(
                        "Microphone delivered non-audio media".into(),
                    ));
                }
                let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(&description)
                    .as_ref()
                    .ok_or_else(|| {
                        CaptureError::Native("Microphone sample has no audio description".into())
                    })?;
                if asbd.mFormatID != LINEAR_PCM
                    || asbd.mSampleRate != f64::from(sample_rate)
                    || asbd.mChannelsPerFrame != u32::from(channels)
                    || buffer.data_buffer().is_none()
                {
                    return Err(CaptureError::UnsupportedFormat(format!(
                        "Microphone cannot deliver {sample_rate} Hz linear PCM, {channels} channels; negotiated {} Hz, {} channels (format {:#x}). Select another format or device",
                        asbd.mSampleRate, asbd.mChannelsPerFrame, asbd.mFormatID
                    )));
                }
                Ok(SampleKind::Audio)
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct VideoFormat {
    width: u32,
    height: u32,
    min_fps: f64,
    max_fps: f64,
}

fn choose_video_format(
    formats: &[VideoFormat],
    width: u32,
    height: u32,
    fps: u32,
) -> Option<usize> {
    if width == 0 || height == 0 || fps == 0 {
        return None;
    }
    formats
        .iter()
        .enumerate()
        .filter(|(_, format)| {
            format.width >= width
                && format.height >= height
                && format.min_fps.is_finite()
                && format.max_fps.is_finite()
                && format.min_fps > 0.0
                && format.min_fps <= format.max_fps
                && format.max_fps >= f64::from(fps)
        })
        .min_by(|(_, a), (_, b)| {
            (u64::from(a.width) * u64::from(a.height))
                .cmp(&(u64::from(b.width) * u64::from(b.height)))
                .then_with(|| {
                    a.min_fps
                        .max(f64::from(fps))
                        .total_cmp(&b.min_fps.max(f64::from(fps)))
                })
        })
        .map(|(index, _)| index)
}

fn camera_output_settings(
    width: u32,
    height: u32,
) -> Result<Retained<NSDictionary<NSString, AnyObject>>, CaptureError> {
    // macOS uncompressed video output accepts Core Video dimensions and an
    // AVFoundation scaling mode. Conversion belongs to the output, not the
    // camera input; aspect-fit preserves the whole image without distortion.
    unsafe {
        let (Some(scaling_key), Some(scaling_mode)) =
            (AVVideoScalingModeKey, AVVideoScalingModeResizeAspect)
        else {
            return Err(CaptureError::UnsupportedFormat(
                "Camera output scaling is unavailable on this system".into(),
            ));
        };
        let pixel_key = NSString::from_str(&kCVPixelBufferPixelFormatTypeKey.to_string());
        let width_key = NSString::from_str(&kCVPixelBufferWidthKey.to_string());
        let height_key = NSString::from_str(&kCVPixelBufferHeightKey.to_string());
        let pixel = NSNumber::new_u32(kCVPixelFormatType_32BGRA);
        let width = NSNumber::new_u32(width);
        let height = NSNumber::new_u32(height);
        Ok(NSDictionary::from_slices(
            &[&*pixel_key, &*width_key, &*height_key, scaling_key],
            &[
                pixel.as_ref(),
                width.as_ref(),
                height.as_ref(),
                scaling_mode.as_ref(),
            ],
        ))
    }
}

fn configure_camera(
    device: &AVCaptureDevice,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<(), CaptureError> {
    // SAFETY: Device configuration is exclusively on the registry worker.
    unsafe {
        let mut candidates = Vec::new();
        let mut native: Vec<(Retained<AVCaptureDeviceFormat>, CMTime)> = Vec::new();
        for format in device.formats() {
            // These CoreMedia selectors are not exposed by AVFoundation's
            // default feature set. The description is borrowed from format.
            let description: *const CMFormatDescription = msg_send![&*format, formatDescription];
            let Some(description) = description.as_ref() else {
                continue;
            };
            if description.media_type() != kCMMediaType_Video {
                continue;
            }
            let dimensions = CMVideoFormatDescriptionGetDimensions(description);
            if dimensions.width <= 0 || dimensions.height <= 0 {
                continue;
            }
            for range in format.videoSupportedFrameRateRanges() {
                candidates.push(VideoFormat {
                    width: dimensions.width as u32,
                    height: dimensions.height as u32,
                    min_fps: range.minFrameRate(),
                    max_fps: range.maxFrameRate(),
                });
                let duration = if f64::from(fps) < range.minFrameRate() {
                    msg_send![&*range, maxFrameDuration]
                } else {
                    CMTime {
                        value: 1,
                        timescale: fps as i32,
                        flags: CMTimeFlags::Valid,
                        epoch: 0,
                    }
                };
                native.push((format.clone(), duration));
            }
        }
        let Some(index) = choose_video_format(&candidates, width, height, fps) else {
            // Output sizing is independent of the camera's native formats.
            // Preserve a working session-selected format when no exact-capacity
            // input is available rather than rejecting an otherwise usable device.
            log::warn!(
                "Camera {} will use its session-selected format for {width}x{height} at {fps} fps; native cadence may be lower",
                device.localizedName()
            );
            return Ok(());
        };
        device.lockForConfiguration().map_err(|error| {
            native_error(
                "Could not lock camera configuration; close other camera applications and retry",
                &error,
            )
        })?;
        device.setActiveFormat(&native[index].0);
        let _: () = msg_send![device, setActiveVideoMinFrameDuration: native[index].1];
        let _: () = msg_send![device, setActiveVideoMaxFrameDuration: native[index].1];
        device.unlockForConfiguration();
        Ok(())
    }
}

fn native_error(context: &str, error: &NSError) -> CaptureError {
    CaptureError::Native(format!(
        "{context}: {} ({} {})",
        error.localizedDescription(),
        error.domain(),
        error.code()
    ))
}

struct Observers {
    center: Retained<NSNotificationCenter>,
    tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl Observers {
    fn new(
        session: &AVCaptureSession,
        device: &AVCaptureDevice,
        state: &Arc<CallbackState>,
    ) -> Self {
        let center = NSNotificationCenter::defaultCenter();
        let mut tokens = Vec::new();
        // SAFETY: Filters refer to live session/device objects retained by the
        // session owner. Copied blocks capture only thread-safe Rust state.
        unsafe {
            for (name, object, message) in [
                (
                    AVCaptureSessionRuntimeErrorNotification,
                    session.as_ref(),
                    "Device capture runtime error",
                ),
                (
                    AVCaptureSessionWasInterruptedNotification,
                    session.as_ref(),
                    "Device capture interrupted; close competing applications and retry",
                ),
                (
                    AVCaptureSessionDidStopRunningNotification,
                    session.as_ref(),
                    "Device capture stopped unexpectedly; check device connection and retry",
                ),
                (
                    AVCaptureDeviceWasDisconnectedNotification,
                    device.as_ref(),
                    "Selected device disconnected; reconnect it and refresh sources",
                ),
            ] {
                let state = state.clone();
                let callback = RcBlock::new(move |notification: NonNull<NSNotification>| {
                    state.callback(|| {
                        let notification = notification.as_ref();
                        let error = notification
                            .userInfo()
                            .and_then(|info| info.objectForKey(AVCaptureSessionErrorKey))
                            .and_then(|error| error.downcast::<NSError>().ok())
                            .map_or_else(
                                || CaptureError::Native(message.into()),
                                |error| native_error(message, &error),
                            );
                        state.fail(error);
                    });
                });
                tokens.push(center.addObserverForName_object_queue_usingBlock(
                    Some(name),
                    Some(object),
                    None,
                    &callback,
                ));
            }
        }
        Self { center, tokens }
    }

    fn remove(&mut self) {
        for token in self.tokens.drain(..) {
            // SAFETY: Token belongs to this center and remains retained here.
            unsafe { self.center.removeObserver((*token).as_ref()) };
        }
    }
}

impl Drop for Observers {
    fn drop(&mut self) {
        self.remove();
    }
}

enum DeviceOutput {
    Video(Retained<AVCaptureVideoDataOutput>),
    Audio(Retained<AVCaptureAudioDataOutput>),
}

impl DeviceOutput {
    fn output(&self) -> &AVCaptureOutput {
        match self {
            Self::Video(output) => output,
            Self::Audio(output) => output,
        }
    }

    fn detach(&self) {
        // SAFETY: Called only by the owner worker; nil delegate permits nil queue.
        unsafe {
            match self {
                Self::Video(output) => output.setSampleBufferDelegate_queue(None, None),
                Self::Audio(output) => output.setSampleBufferDelegate_queue(None, None),
            }
        }
    }
}

struct DeviceSession {
    session: Retained<AVCaptureSession>,
    output: DeviceOutput,
    _device: Retained<AVCaptureDevice>,
    _delegate: Retained<DeviceDelegate>,
    queue: DispatchRetained<DispatchQueue>,
    observers: Observers,
    state: Arc<CallbackState>,
    stopped: bool,
}

impl CaptureSession for DeviceSession {
    fn stop(&mut self) -> Result<(), CaptureError> {
        if self.stopped {
            return Ok(());
        }
        self.state.status().stopped = true;
        self.output.detach();
        self.observers.remove();
        // SAFETY: Native objects cannot cross threads; this owner is never sent
        // to the main/callback queue. Stop first, then drain already queued work.
        unsafe { self.session.stopRunning() };
        self.queue.exec_sync(|| {});
        // Release input ownership even if the caller retains the stopped session.
        unsafe {
            self.session.removeOutput(self.output.output());
            for input in self.session.inputs() {
                self.session.removeInput(&input);
            }
        }
        self.stopped = true;
        Ok(())
    }
}

impl Drop for DeviceSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Called by the capture worker. Does not request permissions or substitute a
/// default device. Sample format negotiation failures are sent to the publisher.
pub fn start(
    request: &CaptureRequest,
    publisher: SamplePublisher<NativeSample>,
) -> Result<Box<dyn CaptureSession>, CaptureError> {
    let state = Arc::new(CallbackState {
        sink: Box::new(publisher),
        format: request.format,
        status: Mutex::new(CallbackStatus::default()),
    });
    let result = autoreleasepool(|_| start_native(request, state.clone()));
    match result {
        Ok(session) => Ok(Box::new(session)),
        Err(error) => {
            // Setup may have torn down a partial native session already.
            state.sink.fail(error.clone());
            Err(error)
        }
    }
}

fn start_native(
    request: &CaptureRequest,
    state: Arc<CallbackState>,
) -> Result<DeviceSession, CaptureError> {
    if MainThreadMarker::new().is_some() {
        return Err(CaptureError::Native(
            "Device capture must start on the capture worker, never the main thread".into(),
        ));
    }
    request.format.validate(&request.source)?;
    let (id, permission, label, media) = match &request.source {
        SourceKey::Camera(id) => (id, permissions::camera_status(), "Camera", unsafe {
            AVMediaTypeVideo
        }),
        SourceKey::Microphone(id) => (id, permissions::microphone_status(), "Microphone", unsafe {
            AVMediaTypeAudio
        }),
        _ => {
            return Err(CaptureError::Unavailable(
                "AVFoundation requires a camera or microphone source".into(),
            ));
        }
    };
    if permission != AVAuthorizationStatus::Authorized {
        return Err(CaptureError::Permission(format!(
            "{label} access is {}; grant Oculi access in System Settings > Privacy & Security > {label}, then retry (capture never prompts)",
            permissions::status_label(permission)
        )));
    }
    let media = media
        .ok_or_else(|| CaptureError::Unavailable(format!("{label} media type unavailable")))?;
    // SAFETY: Authorization was checked before opening the exact persistent ID.
    // All native session ownership stays on this worker; callbacks own only state.
    unsafe {
        let device =
            AVCaptureDevice::deviceWithUniqueID(&NSString::from_str(id)).ok_or_else(|| {
                CaptureError::Unavailable(format!(
                    "{label} {id} not found; reconnect it and refresh sources"
                ))
            })?;
        if !device.isConnected()
            || !device.hasMediaType(media)
            || device.uniqueID().to_string() != *id
        {
            return Err(CaptureError::Unavailable(format!(
                "Selected {label} {id} is unavailable or has the wrong media type"
            )));
        }
        if device.isSuspended() {
            return Err(CaptureError::Unavailable(format!(
                "{label} {id} is suspended; open its privacy shutter or wake/reconnect it"
            )));
        }
        let input =
            AVCaptureDeviceInput::deviceInputWithDevice_error(&device).map_err(|error| {
                native_error(
                    &format!(
                        "Cannot open {label} {id}; close other applications using it and retry"
                    ),
                    &error,
                )
            })?;
        let session = AVCaptureSession::new();
        if !session.canAddInput(&input) {
            return Err(CaptureError::Unavailable(format!(
                "Cannot attach {label} {id}; close competing capture applications and retry"
            )));
        }
        session.addInput(&input);
        let queue = DispatchQueue::new("oculi.device.samples", None);
        let delegate = DeviceDelegate::new(state.clone(), session.clone());
        let output = match request.format {
            CaptureFormat::Video { width, height, fps } => {
                if session.canSetSessionPreset(AVCaptureSessionPresetInputPriority) {
                    session.beginConfiguration();
                    session.setSessionPreset(AVCaptureSessionPresetInputPriority);
                    let configured = configure_camera(&device, width, height, fps);
                    session.commitConfiguration();
                    configured?;
                } else {
                    // Some cameras only support session-selected native formats.
                    // Do not send active-format/frame-duration setters to them.
                    log::info!(
                        "Camera {} uses automatic input format negotiation",
                        device.localizedName()
                    );
                }
                let output = AVCaptureVideoDataOutput::new();
                if !session.canAddOutput(&output) {
                    return Err(CaptureError::UnsupportedFormat(
                        "Cannot attach camera video data output".into(),
                    ));
                }
                session.addOutput(&output);
                if !output
                    .availableVideoCVPixelFormatTypes()
                    .iter()
                    .any(|value| value.unsignedIntValue() == kCVPixelFormatType_32BGRA)
                {
                    return Err(CaptureError::UnsupportedFormat(
                        "Camera cannot produce BGRA; select another camera format".into(),
                    ));
                }
                let settings = camera_output_settings(width, height)?;
                output.setVideoSettings(Some(&settings));
                output.setAlwaysDiscardsLateVideoFrames(true);
                output.setSampleBufferDelegate_queue(
                    Some(ProtocolObject::from_ref(&*delegate)),
                    Some(&queue),
                );
                DeviceOutput::Video(output)
            }
            CaptureFormat::Audio {
                sample_rate,
                channels,
            } => {
                let output = AVCaptureAudioDataOutput::new();
                if !session.canAddOutput(&output) {
                    return Err(CaptureError::UnsupportedFormat(
                        "Cannot attach microphone audio data output".into(),
                    ));
                }
                session.addOutput(&output);
                let format = NSNumber::new_u32(LINEAR_PCM);
                let rate = NSNumber::new_f64(f64::from(sample_rate));
                let channels = NSNumber::new_u16(channels);
                let depth = NSNumber::new_u32(32);
                let yes = NSNumber::new_bool(true);
                let no = NSNumber::new_bool(false);
                let settings = NSDictionary::<NSString, AnyObject>::from_slices(
                    &[
                        AVFormatIDKey,
                        AVSampleRateKey,
                        AVNumberOfChannelsKey,
                        AVLinearPCMBitDepthKey,
                        AVLinearPCMIsFloatKey,
                        AVLinearPCMIsBigEndianKey,
                        AVLinearPCMIsNonInterleaved,
                    ],
                    &[
                        format.as_ref(),
                        rate.as_ref(),
                        channels.as_ref(),
                        depth.as_ref(),
                        yes.as_ref(),
                        no.as_ref(),
                        no.as_ref(),
                    ],
                );
                output.setAudioSettings(Some(&settings));
                output.setSampleBufferDelegate_queue(
                    Some(ProtocolObject::from_ref(&*delegate)),
                    Some(&queue),
                );
                DeviceOutput::Audio(output)
            }
        };
        let observers = Observers::new(&session, &device, &state);
        let owned = DeviceSession {
            session,
            output,
            _device: device,
            _delegate: delegate,
            queue,
            observers,
            state,
            stopped: false,
        };
        if owned
            .output
            .output()
            .connectionWithMediaType(media)
            .is_none()
        {
            return Err(CaptureError::Unavailable(format!(
                "No capture connection for {label} {id}"
            )));
        }
        owned.session.startRunning();
        if let Some(error) = owned.state.status().error.clone() {
            return Err(error);
        }
        if !owned.session.isRunning() {
            return Err(CaptureError::Native(format!(
                "{label} {id} failed to start; close other applications, check permissions, and reconnect the device"
            )));
        }
        Ok(owned)
    }
}

// This sink observes actual validated, retained AVFoundation samples. It does
// not replace any native setup/callback code or fabricate successful capture.
struct SmokeSink(mpsc::SyncSender<Result<NativeSample, CaptureError>>);

impl SampleSink for SmokeSink {
    fn publish(&self, sample: NativeSample) {
        let _ = self.0.try_send(Ok(sample));
    }
    fn fail(&self, error: CaptureError) {
        let _ = self.0.try_send(Err(error));
    }
    fn note_drop(&self) {}
}

fn validate_smoke_metadata(
    kind: SampleKind,
    expected: SampleKind,
    pts: f64,
    previous: Option<f64>,
) -> Result<(), String> {
    if kind != expected {
        return Err(format!("expected {expected:?} sample, received {kind:?}"));
    }
    if !pts.is_finite() || previous.is_some_and(|previous| pts <= previous) {
        return Err(format!(
            "PTS must be finite and strictly advancing: previous {previous:?}, received {pts}"
        ));
    }
    Ok(())
}

/// Explicit, non-prompting native check for bundled CLI callers. Blocks while a
/// dedicated worker captures three samples per format and verifies idempotent,
/// quiescent stop. Cameras exercise standard and non-native output sizes.
/// Missing permission or hardware is reported as SKIP, never as verified capture.
pub fn smoke_check(camera: bool) -> Result<String, String> {
    if !camera {
        return smoke_check_format(CaptureFormat::Audio {
            sample_rate: 48_000,
            channels: 1,
        });
    }
    let mut reports = Vec::new();
    for (width, height, fps) in [(640, 480, 15), (1920, 1080, 30), (1000, 560, 30)] {
        let report = smoke_check_format(CaptureFormat::Video { width, height, fps })?;
        if report.starts_with("SKIP") {
            return Ok(report);
        }
        reports.push(report);
    }
    Ok(reports.join("\n"))
}

fn smoke_check_format(format: CaptureFormat) -> Result<String, String> {
    let camera = matches!(format, CaptureFormat::Video { .. });
    let label = if camera { "camera" } else { "microphone" };
    std::thread::Builder::new()
        .name(format!("oculi.{label}.smoke"))
        .spawn(move || autoreleasepool(|_| {
            let permission = if camera {
                permissions::camera_status()
            } else {
                permissions::microphone_status()
            };
            if permission != AVAuthorizationStatus::Authorized {
                return Ok(format!(
                    "SKIP {label} native capture: permission {}; no prompt issued and capture NOT verified",
                    permissions::status_label(permission)
                ));
            }
            let variable = if camera {
                "OCULI_CAMERA_DEVICE_ID"
            } else {
                "OCULI_MICROPHONE_DEVICE_ID"
            };
            // SAFETY: Only inspect availability after authorization; no access request is made.
            let id = unsafe {
                let Some(media) = (if camera { AVMediaTypeVideo } else { AVMediaTypeAudio }) else {
                    return Ok(format!("SKIP {label}: media type unavailable; capture NOT verified"));
                };
                let selected = std::env::var(variable).ok();
                let device = match &selected {
                    Some(id) => AVCaptureDevice::deviceWithUniqueID(&NSString::from_str(id)),
                    None => AVCaptureDevice::defaultDeviceWithMediaType(media),
                };
                let Some(device) = device else {
                    return Ok(format!(
                        "SKIP {label}: no device found (selected {selected:?}); connect a device or set {variable}; capture NOT verified"
                    ));
                };
                if !device.isConnected() || !device.hasMediaType(media) || device.isSuspended() {
                    return Ok(format!(
                        "SKIP {label}: device {} disconnected, suspended or wrong media type; capture NOT verified",
                        device.uniqueID()
                    ));
                }
                device.uniqueID().to_string()
            };
            let request = CaptureRequest {
                source: if camera {
                    SourceKey::Camera(id)
                } else {
                    SourceKey::Microphone(id)
                },
                format,
            };
            let (tx, rx) = mpsc::sync_channel(4);
            let state = Arc::new(CallbackState {
                sink: Box::new(SmokeSink(tx)),
                format: request.format,
                status: Mutex::new(CallbackStatus::default()),
            });
            let mut session = start_native(&request, state.clone())
                .map_err(|error| format!("{label} start failed: {error}"))?;
            let input_description = if camera {
                // SAFETY: Read the worker-owned device's negotiated input only;
                // this is deliberately separate from the validated output size.
                unsafe {
                    let active = session._device.activeFormat();
                    let description: *const CMFormatDescription = msg_send![&*active, formatDescription];
                    description.as_ref().map(|description| {
                        let size = CMVideoFormatDescriptionGetDimensions(description);
                        format!("native input {}x{}, preset {}", size.width, size.height, session.session.sessionPreset())
                    }).unwrap_or_else(|| "native input dimensions unavailable".into())
                }
            } else {
                "native audio input".into()
            };
            let capture_result = (|| {
                let expected = if camera { SampleKind::Video } else { SampleKind::Audio };
                let mut previous = None;
                // receive() validates media, BGRA/PCM format, dimensions/channels,
                // nonempty data and numeric native PTS before this real sample arrives.
                for index in 1..=3 {
                    let sample = rx.recv_timeout(Duration::from_secs(10))
                        .map_err(|error| format!("{label} sample {index}/3 not delivered: {error}"))?
                        .map_err(|error| format!("{label} capture failed: {error}"))?;
                    let pts = sample.timestamp_seconds();
                    validate_smoke_metadata(sample.kind(), expected, pts, previous)
                        .map_err(|error| format!("{label} sample {index}/3 invalid: {error}"))?;
                    previous = Some(pts);
                }
                Ok::<(), String>(())
            })();
            // Do not propagate capture failure until both stop attempts have run.
            let stop_result = session.stop();
            let repeated_stop_result = session.stop();
            let mut errors = Vec::new();
            if let Err(error) = capture_result {
                errors.push(error);
            }
            if let Err(error) = stop_result {
                errors.push(format!("{label} stop failed: {error}"));
            }
            if let Err(error) = repeated_stop_result {
                errors.push(format!("{label} idempotent stop failed: {error}"));
            }
            if let Some(error) = &state.status().error {
                errors.push(format!("{label} native capture failed: {error}"));
            }
            while rx.try_recv().is_ok() {}
            if !matches!(
                rx.recv_timeout(Duration::from_millis(150)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                errors.push(format!("{label} stop was not quiescent: unexpected callback or disconnected sink"));
            }
            if !errors.is_empty() {
                return Err(errors.join("; "));
            }
            Ok(format!(
                "PASS {label}: {format:?}; {input_description}; 3 real native samples, exact output dimensions/format, expected kind, strictly advancing PTS and idempotent/quiescent stop verified"
            ))
        }))
        .map_err(|error| format!("{label} smoke worker could not start: {error}"))?
        .join()
        .map_err(|_| format!("{label} smoke worker panicked; capture NOT verified"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_conversion_settings_are_independent_of_input_dimensions() {
        for (width, height) in [(640, 480), (1920, 1080), (1000, 560)] {
            let settings = camera_output_settings(width, height).unwrap();
            unsafe {
                for (key, expected) in [
                    (kCVPixelBufferWidthKey, width),
                    (kCVPixelBufferHeightKey, height),
                    (kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA),
                ] {
                    let value = settings
                        .objectForKey(&NSString::from_str(&key.to_string()))
                        .unwrap();
                    let actual: u32 = msg_send![&*value, unsignedIntValue];
                    assert_eq!(actual, expected);
                }
                let mode = settings
                    .objectForKey(AVVideoScalingModeKey.unwrap())
                    .unwrap();
                let equal: bool =
                    msg_send![&*mode, isEqual: AVVideoScalingModeResizeAspect.unwrap()];
                assert!(
                    equal,
                    "scaling must preserve aspect ratio, not stretch/crop"
                );
            }
        }
    }

    #[test]
    fn camera_samples_must_match_converted_output_not_native_input() {
        let requested = CaptureFormat::Video {
            width: 1000,
            height: 560,
            fps: 30,
        };
        assert!(matches_video_output(
            1000,
            560,
            kCVPixelFormatType_32BGRA,
            requested
        ));
        for (width, height) in [(1920, 1080), (640, 480), (0, 560), (-1, 560), (1000, 0)] {
            assert!(!matches_video_output(
                width,
                height,
                kCVPixelFormatType_32BGRA,
                requested
            ));
        }
        assert!(!matches_video_output(1000, 560, 0, requested));
        assert!(!matches_video_output(
            1000,
            560,
            kCVPixelFormatType_32BGRA,
            CaptureFormat::Audio {
                sample_rate: 48_000,
                channels: 1
            }
        ));
    }

    #[test]
    fn chooses_smallest_compatible_camera_format() {
        let formats = [
            VideoFormat {
                width: 1920,
                height: 1080,
                min_fps: 1.0,
                max_fps: 60.0,
            },
            VideoFormat {
                width: 1280,
                height: 720,
                min_fps: 1.0,
                max_fps: 30.0,
            },
            VideoFormat {
                width: 640,
                height: 480,
                min_fps: 1.0,
                max_fps: 120.0,
            },
        ];
        assert_eq!(choose_video_format(&formats, 1280, 720, 30), Some(1));
        assert_eq!(choose_video_format(&formats, 1280, 720, 60), Some(0));
        assert_eq!(choose_video_format(&formats, 1920, 1200, 30), None);
        assert_eq!(choose_video_format(&formats, 1920, 1080, 120), None);
    }

    #[test]
    fn camera_ranges_are_not_rounded_or_combined() {
        let formats = [
            VideoFormat {
                width: 1280,
                height: 720,
                min_fps: 29.97,
                max_fps: 29.97,
            },
            VideoFormat {
                width: 1280,
                height: 720,
                min_fps: 60.0,
                max_fps: 60.0,
            },
            VideoFormat {
                width: 1280,
                height: 720,
                min_fps: f64::NAN,
                max_fps: 120.0,
            },
        ];
        assert_eq!(choose_video_format(&formats, 1280, 720, 30), Some(1));
        assert_eq!(choose_video_format(&formats[..1], 1280, 720, 30), None);
        assert_eq!(choose_video_format(&formats, 1280, 720, 0), None);
        assert_eq!(choose_video_format(&[], 1280, 720, 30), None);
    }

    #[test]
    fn rejects_non_numeric_timestamps() {
        let time = CMTime {
            value: 123,
            timescale: 48_000,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        assert!(numeric_time(time));
        assert!(!numeric_time(CMTime {
            timescale: 0,
            ..time
        }));
        assert!(!numeric_time(CMTime {
            flags: CMTimeFlags::Valid | CMTimeFlags::Indefinite,
            ..time
        }));
        assert!(!numeric_time(CMTime {
            flags: CMTimeFlags::empty(),
            ..time
        }));
    }

    fn smoke(camera: bool) {
        let label = if camera { "camera" } else { "microphone" };
        if std::env::var("OCULI_NATIVE_CAPTURE_SMOKE").as_deref() != Ok("1") {
            eprintln!(
                "SKIP {label} native capture: set OCULI_NATIVE_CAPTURE_SMOKE=1 to opt in; capture NOT verified"
            );
            return;
        }
        match smoke_check(camera) {
            Ok(report) => eprintln!("{report}"),
            Err(error) => panic!("{error}"),
        }
    }

    #[test]
    fn smoke_requires_expected_kind_and_strictly_advancing_pts() {
        assert!(validate_smoke_metadata(SampleKind::Video, SampleKind::Video, 0.0, None).is_ok());
        assert!(
            validate_smoke_metadata(SampleKind::Audio, SampleKind::Audio, 1.0, Some(0.0)).is_ok()
        );
        assert!(validate_smoke_metadata(SampleKind::Audio, SampleKind::Video, 1.0, None).is_err());
        for pts in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                validate_smoke_metadata(SampleKind::Video, SampleKind::Video, pts, Some(0.0))
                    .is_err()
            );
        }
    }

    #[test]
    fn native_camera_smoke() {
        smoke(true);
    }

    #[test]
    fn native_microphone_smoke() {
        smoke(false);
    }
}
