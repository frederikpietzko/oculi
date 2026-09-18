//! ScreenCaptureKit sessions. Native streams, outputs and queues are created,
//! operated and released on one lifecycle worker; channels carry Rust only.
//! A timed-out caller closes delivery but does not abandon a pending native
//! start: its worker stops a late success and retains ownership until stopped.
//! Callers must quarantine a source after a start timeout or failed/timed-out
//! stop: cleanup has no completion notification, and retrying can accumulate
//! workers if SCK never completes. A caller deadline is not native cancellation.

use super::{
    application_identity, permissions,
    sample::{NativeSample, SampleKind},
};
use crate::capture::{
    CaptureError, CaptureFormat, CaptureRequest, CaptureSession, SourceKey,
    registry::SamplePublisher,
};
use crate::model::ApplicationProcessId;
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::{
    AnyThread, DefinedClass, available, define_class, msg_send,
    rc::{Retained, autoreleasepool},
    runtime::{AnyObject, ProtocolObject},
};
use objc2_app_kit::NSRunningApplication;
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::kCGColorSpaceSRGB;
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMSampleBuffer, CMTime, kCMMediaType_Audio,
};
use objc2_core_video::kCVPixelFormatType_32BGRA;
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString,
};
use objc2_screen_capture_kit::{
    SCContentFilter, SCFrameStatus, SCRunningApplication, SCShareableContent, SCStream,
    SCStreamConfiguration, SCStreamDelegate, SCStreamFrameInfoStatus, SCStreamOutput,
    SCStreamOutputType,
};
use std::collections::BTreeSet;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(8);
const POLL: Duration = Duration::from_millis(25);
const LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");

struct CallbackState {
    publisher: SamplePublisher<NativeSample>,
    cancelled: AtomicBool,
    start_completed: AtomicBool,
    stopped_after_start: AtomicBool,
    in_flight: AtomicUsize,
    kind: SampleKind,
    format: CaptureFormat,
}

struct CallbackGuard<'a>(&'a CallbackState);
impl Drop for CallbackGuard<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl CallbackState {
    fn callback(&self, action: impl FnOnce()) {
        self.guarded(|| {
            if !self.cancelled.load(Ordering::SeqCst) {
                action();
            }
        });
    }

    fn guarded(&self, action: impl FnOnce()) {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let _guard = CallbackGuard(self);
        if catch_unwind(AssertUnwindSafe(action)).is_err() {
            self.publisher.fail(CaptureError::Native(
                "ScreenCaptureKit callback panicked".into(),
            ));
            self.cancelled.store(true, Ordering::SeqCst);
        }
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "OculiScreenCaptureOutput"]
    #[ivars = Arc<CallbackState>]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn output(&self, _stream: &SCStream, buffer: &CMSampleBuffer, kind: SCStreamOutputType) {
            let state = self.ivars();
            state.callback(|| {
                let expected = output_type(state.kind);
                if kind != expected { return; }
                // SAFETY: SCK owns a live sample for this callback. Attachments
                // are read before publishing; neither we nor consumers mutate it.
                unsafe {
                    if !buffer.is_valid() {
                        state.publisher.note_drop();
                        return;
                    }
                    if kind == SCStreamOutputType::Screen {
                        match frame_status(buffer) {
                            Some(SCFrameStatus::Complete) => {},
                            // Idle is not a new frame. Blank/suspended/started
                            // are transient state, not evidence of source loss.
                            Some(SCFrameStatus::Idle | SCFrameStatus::Blank |
                                SCFrameStatus::Suspended | SCFrameStatus::Started) => return,
                            Some(SCFrameStatus::Stopped) => {
                                state.publisher.fail(CaptureError::Unavailable("Screen stream stopped".into()));
                                state.cancelled.store(true, Ordering::SeqCst);
                                return;
                            },
                            _ => { state.publisher.note_drop(); return; }
                        }
                    }
                    if let CaptureFormat::Audio { sample_rate, channels } = state.format
                        && let Err(error) = validate_audio_sample(buffer, sample_rate, channels)
                    {
                        state.publisher.fail(error);
                        state.cancelled.store(true, Ordering::SeqCst);
                        return;
                    }
                    if let Some(sample) = NativeSample::retain(buffer, state.kind) {
                        state.publisher.publish(sample);
                    } else {
                        state.publisher.note_drop();
                    }
                }
            });
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stopped(&self, _stream: &SCStream, error: &NSError) {
            let state = self.ivars();
            state.guarded(|| {
                if !state.cancelled.load(Ordering::SeqCst) {
                    state.publisher.fail(native_error(error));
                }
                // A delegate stop after start completion is terminal. Do not
                // repeatedly stop an already-stopped stream. An earlier stop
                // cannot prove that a still-pending start won't succeed later.
                if state.start_completed.load(Ordering::SeqCst) {
                    state.stopped_after_start.store(true, Ordering::SeqCst);
                }
                state.cancelled.store(true, Ordering::SeqCst);
            });
        }
    }
);

impl Output {
    fn new(state: Arc<CallbackState>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(state);
        // SAFETY: NSObject initialization, with initialized thread-safe Rust ivars.
        unsafe { msg_send![super(this), init] }
    }
}

fn native_error(error: &NSError) -> CaptureError {
    CaptureError::Native(format!(
        "{} ({}): {}",
        error.domain(),
        error.code(),
        error.localizedDescription()
    ))
}

unsafe fn completion_result(error: *mut NSError) -> Result<(), CaptureError> {
    // SAFETY: Native completion guarantees this optional error for its duration.
    unsafe { error.as_ref() }.map_or(Ok(()), |error| Err(native_error(error)))
}

unsafe fn frame_status(buffer: &CMSampleBuffer) -> Option<SCFrameStatus> {
    // SAFETY: SCK supplies a CFArray of sample attachment dictionaries. CFArray
    // and CFDictionary are toll-free bridged to NSArray/NSDictionary. Values are
    // NSObject-compatible; the status value is dynamically checked as NSNumber.
    // Nothing is mutated and all borrowed pointers remain under the retained array.
    unsafe {
        let attachments = buffer.sample_attachments_array(false)?;
        let array = &*CFRetained::as_ptr(&attachments)
            .as_ptr()
            .cast::<NSArray<NSDictionary<NSString, AnyObject>>>();
        let dictionary = array.firstObject()?;
        let value = dictionary.objectForKey(SCStreamFrameInfoStatus)?;
        let number = value.downcast_ref::<NSNumber>()?;
        Some(SCFrameStatus(number.integerValue()))
    }
}

fn output_type(kind: SampleKind) -> SCStreamOutputType {
    match kind {
        SampleKind::Video => SCStreamOutputType::Screen,
        SampleKind::Audio => SCStreamOutputType::Audio,
    }
}

unsafe fn validate_audio_sample(
    buffer: &CMSampleBuffer,
    sample_rate: u32,
    channels: u16,
) -> Result<(), CaptureError> {
    // SAFETY: Read only during the live SCK callback. No native description or
    // ASBD pointer escapes; only copied scalar metadata enters the validator.
    unsafe {
        let metadata = buffer.format_description().and_then(|description| {
            if description.media_type() != kCMMediaType_Audio {
                return None;
            }
            CMAudioFormatDescriptionGetStreamBasicDescription(&description)
                .as_ref()
                .map(|asbd| (asbd.mFormatID, asbd.mSampleRate, asbd.mChannelsPerFrame))
        });
        validate_audio_metadata(
            metadata,
            buffer.data_buffer().is_some(),
            sample_rate,
            channels,
        )
    }
}

fn validate_audio_metadata(
    delivered: Option<(u32, f64, u32)>,
    has_data: bool,
    sample_rate: u32,
    channels: u16,
) -> Result<(), CaptureError> {
    if has_data && delivered == Some((LINEAR_PCM, f64::from(sample_rate), u32::from(channels))) {
        Ok(())
    } else {
        Err(CaptureError::UnsupportedFormat(format!(
            "ScreenCaptureKit did not deliver {sample_rate} Hz linear PCM, {channels} channels with audio storage; delivered {delivered:?}"
        )))
    }
}

#[derive(Default)]
struct ContentSlot {
    abandoned: bool,
    value: Option<Result<Retained<SCShareableContent>, CaptureError>>,
}

fn shareable_content() -> Result<Retained<SCShareableContent>, CaptureError> {
    // Intentionally crosses only the audited unsafe FFI block boundary below;
    // keep ContentSlot !Send rather than granting general native transfer.
    #[allow(clippy::arc_with_non_send_sync)]
    let slot = Arc::new(Mutex::new(ContentSlot::default()));
    let reply = slot.clone();
    let (tx, rx) = mpsc::sync_channel(1);
    let completion = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            autoreleasepool(|_| {
                let mut slot = reply.lock().unwrap_or_else(|e| e.into_inner());
                if slot.abandoned {
                    return;
                }
                // SAFETY: The callback supplies a valid immutable shareable-content
                // snapshot. Retain before callback return; never retain NSError.
                slot.value = Some(unsafe {
                    completion_result(error).and_then(|()| {
                        Retained::retain(content)
                            .ok_or_else(|| CaptureError::Unavailable("No shareable content".into()))
                    })
                });
                let _ = tx.try_send(());
            });
        },
    );
    // SAFETY AUDIT — concrete, one-way content handoff, NOT a generic Send impl:
    // SCK copies this block and may call it on another thread. Its captures are
    // Arc (atomic ownership), a mutex and a Rust sender. The mutex exclusively
    // transfers ONE retained, immutable SCShareableContent snapshot. The native
    // callback only retains it; only this worker reads its readonly properties.
    // No alias mutates the snapshot or its object graph concurrently with or
    // after this handoff. Objective-C retain/release are thread-safe; no borrowed
    // backing storage or thread-affine UI object in the snapshot. SCStream and
    // its configuration/filter/delegate are NOT transferred this way. Timeout
    // marks the slot abandoned under the same lock, so a late callback retains
    // nothing and cannot leave an active stream. This local block boundary is
    // intentionally the sole exception to native worker confinement.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false, false, &completion);
    }
    let received = rx.recv_timeout(TIMEOUT);
    let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
    slot.abandoned = true;
    if received.is_err() {
        slot.value.take();
        return Err(CaptureError::Timeout("Shareable content".into()));
    }
    slot.value
        .take()
        .unwrap_or_else(|| Err(CaptureError::Native("Missing content completion".into())))
}

fn unique_pid(pids: impl IntoIterator<Item = i32>) -> Result<i32, CaptureError> {
    let pids: BTreeSet<_> = pids.into_iter().filter(|pid| *pid > 0).collect();
    if pids.len() != 1 {
        return Err(CaptureError::Unavailable(
            "Application is absent or has ambiguous running instances".into(),
        ));
    }
    Ok(*pids.first().expect("one PID"))
}

unsafe fn application(
    content: &SCShareableContent,
    bundle: &str,
) -> Result<Retained<SCRunningApplication>, CaptureError> {
    if bundle.is_empty() {
        return Err(CaptureError::Unavailable("Empty application bundle".into()));
    }
    // SAFETY: Snapshot properties are read only on the lifecycle worker.
    unsafe {
        let apps: Vec<_> = application_identity::shareable_applications(content)
            .into_iter()
            .filter(|app| {
                application_identity::resolve_metadata(
                    app.processID(),
                    app.bundleIdentifier().to_string(),
                    app.applicationName().to_string(),
                )
                .0 == bundle
            })
            .collect();
        let running = NSRunningApplication::runningApplicationsWithBundleIdentifier(
            &NSString::from_str(bundle),
        );
        let pid = unique_pid(
            apps.iter().map(|app| app.processID()).chain(
                running
                    .iter()
                    .filter(|app| !app.isTerminated())
                    .map(|app| app.processIdentifier()),
            ),
        )?;
        if pid == std::process::id() as i32 {
            return Err(CaptureError::Unavailable(
                "Oculi cannot capture its own application playback".into(),
            ));
        }
        apps.into_iter()
            .find(|app| app.processID() == pid)
            .ok_or_else(|| {
                CaptureError::Unavailable(format!("Application {bundle} is not shareable"))
            })
    }
}

fn validate_process(process: &ApplicationProcessId) -> Result<(), CaptureError> {
    if process.pid == std::process::id() as i32 {
        return Err(CaptureError::Unavailable(
            "Oculi cannot capture its own application".into(),
        ));
    }
    if !application_identity::is_current(process) {
        return Err(CaptureError::Unavailable(format!(
            "Application process PID {} ended or changed; reselect the application",
            process.pid,
        )));
    }
    Ok(())
}

unsafe fn process_application(
    content: &SCShareableContent,
    process: &ApplicationProcessId,
) -> Result<Retained<SCRunningApplication>, CaptureError> {
    validate_process(process)?;
    // SAFETY: Snapshot children stay retained on this lifecycle worker. Select
    // the exact owning process, including owners only reported through windows.
    let app = unsafe { application_identity::shareable_applications(content) }
        .into_iter()
        .find(|app| unsafe { app.processID() } == process.pid)
        .ok_or_else(|| {
            CaptureError::Unavailable(format!(
                "Application process PID {} is not shareable",
                process.pid,
            ))
        })?;
    validate_process(process)?;
    Ok(app)
}

fn selected_process(source: &SourceKey) -> Option<&ApplicationProcessId> {
    match source {
        SourceKey::ApplicationProcessVideo { process, .. }
        | SourceKey::ApplicationProcessAudio(process) => Some(process),
        _ => None,
    }
}

unsafe fn filter(
    content: &SCShareableContent,
    source: &SourceKey,
) -> Result<Retained<SCContentFilter>, CaptureError> {
    // SAFETY: All objects are retained locally and used only on this worker.
    unsafe {
        if let SourceKey::Window { id, owner } = source {
            let window = content
                .windows()
                .iter()
                .find(|window| window.windowID() == *id)
                .ok_or_else(|| {
                    CaptureError::Unavailable(format!("Window {id} no longer exists"))
                })?;
            if let Some(owner) = owner
                && window
                    .owningApplication()
                    .as_ref()
                    .map(|app| app.bundleIdentifier().to_string())
                    .as_ref()
                    != Some(owner)
            {
                return Err(CaptureError::Unavailable(
                    "Window owner changed; reselect the window".into(),
                ));
            }
            return Ok(SCContentFilter::initWithDesktopIndependentWindow(
                SCContentFilter::alloc(),
                &window,
            ));
        }
        let display_id = match source {
            SourceKey::Display(id) => Some(*id),
            SourceKey::ApplicationVideo { display, .. }
            | SourceKey::ApplicationProcessVideo { display, .. } => Some(*display),
            SourceKey::ApplicationAudio(_)
            | SourceKey::ApplicationProcessAudio(_)
            | SourceKey::SystemAudio => None,
            _ => {
                return Err(CaptureError::Unavailable(
                    "Not a ScreenCaptureKit source".into(),
                ));
            }
        };
        let display = content
            .displays()
            .iter()
            .filter(|display| display_id.is_none_or(|id| display.displayID() == id))
            .min_by_key(|display| display.displayID())
            .ok_or_else(|| CaptureError::Unavailable("Selected display is unavailable".into()))?;
        let empty = NSArray::new();
        let app = match source {
            SourceKey::ApplicationVideo { bundle, .. } | SourceKey::ApplicationAudio(bundle) => {
                Some(application(content, bundle)?)
            }
            SourceKey::ApplicationProcessVideo { process, .. }
            | SourceKey::ApplicationProcessAudio(process) => {
                Some(process_application(content, process)?)
            }
            _ => None,
        };
        if let Some(app) = app {
            // Include all shareable windows of the selected process, including
            // later popups. Audio is app-wide, independently of this display.
            Ok(
                SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &NSArray::from_retained_slice(&[app]),
                    &empty,
                ),
            )
        } else {
            Ok(SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &empty,
            ))
        }
    }
}

unsafe fn configuration(format: CaptureFormat) -> Retained<SCStreamConfiguration> {
    // SAFETY: Validated request; configure before creating the stream. BGRA and
    // the default SDR dynamic range are used (no newer HDR-only APIs required).
    // The color-space name is a process-lifetime framework constant.
    unsafe {
        let config = SCStreamConfiguration::new();
        config.setQueueDepth(3);
        config.setPixelFormat(kCVPixelFormatType_32BGRA);
        config.setColorSpaceName(kCGColorSpaceSRGB);
        config.setScalesToFit(true);
        config.setExcludesCurrentProcessAudio(true);
        match format {
            CaptureFormat::Video { width, height, fps } => {
                config.setWidth(width as usize);
                config.setHeight(height as usize);
                config.setMinimumFrameInterval(CMTime::new(1, fps as i32));
                config.setCapturesAudio(false);
                config.setShowsCursor(true);
            }
            CaptureFormat::Audio { channels, .. } => {
                config.setWidth(2);
                config.setHeight(2);
                config.setMinimumFrameInterval(CMTime::new(1, 1));
                config.setShowsCursor(false);
                config.setCapturesAudio(true);
                config.setSampleRate(48_000);
                config.setChannelCount(channels as isize);
            }
        }
        config
    }
}

struct NativeStream {
    stream: Retained<SCStream>,
    output: Retained<Output>,
    queue: DispatchRetained<DispatchQueue>,
    kind: SCStreamOutputType,
}

impl NativeStream {
    fn new(request: &CaptureRequest, state: Arc<CallbackState>) -> Result<Self, CaptureError> {
        let content = shareable_content()?;
        if state.cancelled.load(Ordering::SeqCst) {
            return Err(CaptureError::Timeout(
                "Capture cancelled before stream creation".into(),
            ));
        }
        // SAFETY: Native objects are created and retained on this worker. Output
        // callback ivars are exclusively thread-safe Rust state. SCK borrows its
        // own stream only during callbacks; those callbacks never operate it.
        unsafe {
            let filter = filter(&content, &request.source)?;
            let config = configuration(request.format);
            let kind = output_type(state.kind);
            let output = Output::new(state);
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*output)),
            );
            let queue = DispatchQueue::new("oculi.screen.samples", None);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    kind,
                    Some(&queue),
                )
                .map_err(|e| native_error(&e))?;
            Ok(Self {
                stream,
                output,
                queue,
                kind,
            })
        }
    }

    fn detach(&self) -> Result<(), CaptureError> {
        // SAFETY: Only the worker calls this; the output stays retained through
        // removal and until the stream (including its weak delegate) is released.
        unsafe {
            self.stream
                .removeStreamOutput_type_error(ProtocolObject::from_ref(&*self.output), self.kind)
        }
        .map_err(|e| native_error(&e))
    }
}

enum Event {
    Started(Result<(), CaptureError>),
    Stopped {
        after_start: bool,
        result: Result<(), CaptureError>,
    },
}

struct StopState {
    starting: bool,
    before_start_requested: bool,
    after_start_pending: bool,
}

impl StopState {
    fn new() -> Self {
        Self {
            starting: true,
            before_start_requested: false,
            after_start_pending: false,
        }
    }

    fn next_stop(&mut self, cancelled: bool) -> Option<bool> {
        if !cancelled {
            return None;
        }
        if self.starting && !self.before_start_requested {
            self.before_start_requested = true;
            Some(false)
        } else if !self.starting && !self.after_start_pending {
            self.after_start_pending = true;
            Some(true)
        } else {
            None
        }
    }

    fn completed(&mut self, after_start: bool, succeeded: bool) -> bool {
        if after_start {
            self.after_start_pending = false;
        }
        if !after_start && !succeeded {
            self.before_start_requested = false;
        }
        after_start && succeeded
    }
}

fn request_stop(stream: &SCStream, tx: &mpsc::Sender<Event>, after_start: bool) {
    let tx = tx.clone();
    let completion = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: Only the owned Rust error crosses the channel.
        let _ = tx.send(Event::Stopped {
            after_start,
            result: unsafe { completion_result(error) },
        });
    });
    // SAFETY: Worker owns the stream; completion captures only a Rust sender.
    autoreleasepool(|_| unsafe { stream.stopCaptureWithCompletionHandler(Some(&completion)) });
}

fn run(
    request: CaptureRequest,
    state: Arc<CallbackState>,
    ready: mpsc::SyncSender<Result<(), CaptureError>>,
    done: mpsc::SyncSender<Result<(), CaptureError>>,
) {
    let native = match autoreleasepool(|_| NativeStream::new(&request, state.clone())) {
        Ok(native) => native,
        Err(error) => {
            let _ = ready.try_send(Err(error));
            let _ = done.try_send(Ok(()));
            return;
        }
    };
    let (tx, rx) = mpsc::channel();
    let start_tx = tx.clone();
    let start_state = state.clone();
    let completion = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: Native error is borrowed only in its completion.
        let result = unsafe { completion_result(error) };
        if result.is_ok() {
            start_state.start_completed.store(true, Ordering::SeqCst);
        }
        let _ = start_tx.send(Event::Started(result));
    });
    // SAFETY: Stream and all lifecycle calls remain on this worker.
    autoreleasepool(|_| unsafe {
        native
            .stream
            .startCaptureWithCompletionHandler(Some(&completion))
    });
    let mut stop = StopState::new();
    let mut retry_at = Instant::now();
    let mut failure = None;
    let mut detached = None;
    let mut check_process_at = Instant::now();
    loop {
        if !state.cancelled.load(Ordering::SeqCst) && Instant::now() >= check_process_at {
            check_process_at = Instant::now() + Duration::from_millis(250);
            if let Some(process) = selected_process(&request.source)
                && let Err(error) = validate_process(process)
            {
                state.publisher.fail(error.clone());
                let _ = ready.try_send(Err(error));
                state.cancelled.store(true, Ordering::SeqCst);
            }
        }
        if state.stopped_after_start.load(Ordering::SeqCst) && !stop.starting {
            let _ = ready.try_send(Err(CaptureError::Unavailable(
                "Screen stream stopped during startup".into(),
            )));
            break;
        }
        if state.cancelled.load(Ordering::SeqCst) && detached.is_none() {
            detached = Some(autoreleasepool(|_| native.detach()));
        }
        if Instant::now() >= retry_at
            && let Some(after_start) = stop.next_stop(state.cancelled.load(Ordering::SeqCst))
        {
            request_stop(&native.stream, &tx, after_start);
        }
        match rx.recv_timeout(POLL) {
            Ok(Event::Started(result)) => {
                stop.starting = false;
                retry_at = Instant::now();
                match result {
                    Ok(()) => {
                        if !state.cancelled.load(Ordering::SeqCst)
                            && ready.try_send(Ok(())).is_err()
                        {
                            state.cancelled.store(true, Ordering::SeqCst);
                        }
                        // A stop issued before start completed is not sufficient:
                        // the next stop must be issued AFTER a late start success.
                    }
                    Err(error) => {
                        let _ = ready.try_send(Err(error));
                        state.cancelled.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
            Ok(Event::Stopped {
                after_start,
                result,
            }) => {
                if stop.completed(after_start, result.is_ok()) {
                    break;
                }
                if let Err(error) = result {
                    // Stopping before start may legitimately report "not
                    // running". Only a post-start failure is a stop failure.
                    if after_start {
                        failure.get_or_insert_with(|| error.clone());
                        let _ = done.try_send(Err(error));
                    }
                    // Keep the native owner alive and retry, rather than
                    // dropping an active stream after a failed stop.
                    retry_at = Instant::now() + Duration::from_millis(250);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => unreachable!("worker owns event sender"),
        }
    }
    state.cancelled.store(true, Ordering::SeqCst);
    let detached = detached.unwrap_or_else(|| autoreleasepool(|_| native.detach()));
    // Drain the serial output queue before releasing the delegate. Only a Rust
    // acknowledgement crosses this boundary. If a framework callback stalls,
    // retain the owner here; Session::stop still has its independent deadline.
    let (drained_tx, drained_rx) = mpsc::sync_channel(1);
    native.queue.exec_async(move || {
        let _ = drained_tx.try_send(());
    });
    let _ = drained_rx.recv();
    // Once delivery is closed, later callback entries do no work. Existing
    // callbacks hold an in-flight guard. Keep native delegate ownership until
    // every such callback exits; caller waits are independently bounded.
    while state.in_flight.load(Ordering::SeqCst) != 0 {
        std::thread::sleep(POLL);
    }
    autoreleasepool(|_| drop(native));
    let _ = done.try_send(failure.map_or(detached, Err));
}

struct Session {
    state: Arc<CallbackState>,
    done: mpsc::Receiver<Result<(), CaptureError>>,
    worker: Option<JoinHandle<()>>,
    result: Option<Result<(), CaptureError>>,
    _confined: PhantomData<Rc<()>>,
}

impl CaptureSession for Session {
    fn stop(&mut self) -> Result<(), CaptureError> {
        self.state.cancelled.store(true, Ordering::SeqCst);
        if let Some(result) = &self.result {
            return result.clone();
        }
        let deadline = Instant::now() + TIMEOUT;
        let mut result = self.done.recv_timeout(TIMEOUT).unwrap_or_else(|error| {
            Err(match error {
                mpsc::RecvTimeoutError::Timeout => CaptureError::Timeout(
                    "Screen stream stop; cleanup worker retains ownership".into(),
                ),
                mpsc::RecvTimeoutError::Disconnected => {
                    CaptureError::Native("Screen lifecycle worker disconnected".into())
                }
            })
        });
        // Also quiesce publication when a failed native stop reports early.
        // New callback entries see cancelled and cannot publish.
        while self.state.in_flight.load(Ordering::SeqCst) != 0 {
            if Instant::now() >= deadline {
                result = Err(CaptureError::Timeout(
                    "Screen callbacks did not quiesce".into(),
                ));
                break;
            }
            std::thread::sleep(POLL);
        }
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.is_finished())
            && let Some(worker) = self.worker.take()
        {
            let _ = worker.join();
        }
        self.result = Some(result.clone());
        result
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub fn start(
    request: &CaptureRequest,
    publisher: SamplePublisher<NativeSample>,
) -> Result<Box<dyn CaptureSession>, CaptureError> {
    request.format.validate(&request.source)?;
    if !matches!(
        request.source,
        SourceKey::Display(_)
            | SourceKey::Window { .. }
            | SourceKey::ApplicationVideo { .. }
            | SourceKey::ApplicationAudio(_)
            | SourceKey::ApplicationProcessVideo { .. }
            | SourceKey::ApplicationProcessAudio(_)
            | SourceKey::SystemAudio
    ) {
        return Err(CaptureError::Unavailable(
            "Not a ScreenCaptureKit source".into(),
        ));
    }
    if !available!(macos = 13.0) {
        return Err(CaptureError::Unavailable(
            "Screen capture requires macOS 13 or newer".into(),
        ));
    }
    if !permissions::screen_granted() {
        return Err(CaptureError::Permission(
            "Screen & System Audio Recording; grant access explicitly in System Settings".into(),
        ));
    }
    let state = Arc::new(CallbackState {
        publisher,
        cancelled: AtomicBool::new(false),
        start_completed: AtomicBool::new(false),
        stopped_after_start: AtomicBool::new(false),
        in_flight: AtomicUsize::new(0),
        kind: if request.source.is_audio() {
            SampleKind::Audio
        } else {
            SampleKind::Video
        },
        format: request.format,
    });
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let worker_state = state.clone();
    let request = request.clone();
    let worker = std::thread::Builder::new()
        .name("screen-capture-lifecycle".into())
        .spawn(move || run(request, worker_state, ready_tx, done_tx))
        .map_err(|error| CaptureError::Native(format!("Cannot create screen worker: {error}")))?;
    match ready_rx.recv_timeout(TIMEOUT) {
        Ok(Ok(())) => Ok(Box::new(Session {
            state,
            done: done_rx,
            worker: Some(worker),
            result: None,
            _confined: PhantomData,
        })),
        result => {
            state.cancelled.store(true, Ordering::SeqCst);
            // Do not join here: start completion may be arbitrarily late. The
            // worker still owns every native object and stops any late success.
            Err(match result {
                Ok(Err(error)) => error,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    CaptureError::Timeout("Screen stream start".into())
                }
                _ => CaptureError::Native("Screen lifecycle worker disconnected".into()),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{
        CaptureBackend,
        registry::{CaptureRegistry, SubscriptionPolicy},
    };

    struct Backend;
    impl CaptureBackend<NativeSample> for Backend {
        fn start(
            &mut self,
            request: &CaptureRequest,
            publisher: SamplePublisher<NativeSample>,
        ) -> Result<Box<dyn CaptureSession>, CaptureError> {
            start(request, publisher)
        }
    }

    #[test]
    fn audio_format_must_match_requested_pcm() {
        for channels in [1, 2] {
            assert!(
                validate_audio_metadata(
                    Some((LINEAR_PCM, 48_000.0, u32::from(channels))),
                    true,
                    48_000,
                    channels,
                )
                .is_ok()
            );
        }
        for metadata in [
            None,
            Some((LINEAR_PCM, 44_100.0, 2)),
            Some((LINEAR_PCM, 48_000.0, 1)),
            Some((LINEAR_PCM, 48_000.0, 0)),
            Some((LINEAR_PCM, f64::NAN, 2)),
            Some((LINEAR_PCM, f64::INFINITY, 2)),
            Some((u32::from_be_bytes(*b"aac "), 48_000.0, 2)),
        ] {
            assert!(matches!(
                validate_audio_metadata(metadata, true, 48_000, 2),
                Err(CaptureError::UnsupportedFormat(_))
            ));
        }
        assert!(
            validate_audio_metadata(Some((LINEAR_PCM, 48_000.0, 2)), false, 48_000, 2).is_err()
        );
        assert!(validate_audio_metadata(Some((LINEAR_PCM, 48_000.0, 2)), true, 48_000, 1).is_err());
    }

    #[test]
    fn application_identity_is_fail_closed() {
        assert!(unique_pid([]).is_err());
        assert!(unique_pid([0, -1]).is_err());
        assert!(unique_pid([12, 13]).is_err());
        assert_eq!(unique_pid([12, 12]).unwrap(), 12);
    }

    #[test]
    fn process_capture_rejects_own_and_unverifiable_targets() {
        let process = application_identity::process_metadata(std::process::id() as i32)
            .unwrap()
            .id;
        assert!(
            validate_process(&process)
                .unwrap_err()
                .to_string()
                .contains("own application")
        );
        let absent = ApplicationProcessId {
            pid: i32::MAX,
            ..process
        };
        assert!(
            validate_process(&absent)
                .unwrap_err()
                .to_string()
                .contains("reselect")
        );
        for source in [
            SourceKey::ApplicationProcessVideo {
                process,
                display: 1,
            },
            SourceKey::ApplicationProcessAudio(process),
        ] {
            assert_eq!(selected_process(&source), Some(&process));
        }
        assert!(selected_process(&SourceKey::SystemAudio).is_none());
    }

    #[test]
    fn late_start_is_stopped_even_if_early_stop_never_completes() {
        let mut stop = StopState::new();
        assert_eq!(stop.next_stop(false), None);
        assert_eq!(stop.next_stop(true), Some(false));
        assert_eq!(stop.next_stop(true), None);
        stop.starting = false;
        assert_eq!(stop.next_stop(true), Some(true));
        assert!(!stop.completed(false, true));
        assert_eq!(stop.next_stop(true), None);
        assert!(stop.completed(true, true));
    }

    #[test]
    fn early_stop_success_does_not_dispose_pending_start() {
        let mut stop = StopState::new();
        assert_eq!(stop.next_stop(true), Some(false));
        assert!(!stop.completed(false, true));
        assert_eq!(stop.next_stop(true), None);
        stop.starting = false;
        assert_eq!(stop.next_stop(true), Some(true));
        assert!(stop.completed(true, true));
    }

    #[test]
    fn failed_stop_keeps_owner_and_retries() {
        let mut stop = StopState::new();
        stop.starting = false;
        assert_eq!(stop.next_stop(false), None);
        assert_eq!(stop.next_stop(true), Some(true));
        assert!(!stop.completed(true, false));
        assert_eq!(stop.next_stop(true), Some(true));
        assert!(stop.completed(true, true));
    }

    #[test]
    fn native_screen_capture_smoke() {
        if std::env::var("OCULI_NATIVE_CAPTURE_SMOKE").as_deref() != Ok("1") {
            eprintln!("SKIP native screen capture: set OCULI_NATIVE_CAPTURE_SMOKE=1");
            return;
        }
        if !available!(macos = 13.0) || !permissions::screen_granted() {
            eprintln!(
                "SKIP native screen capture: macOS 13+ and existing Screen Recording permission required; never prompting"
            );
            return;
        }
        let sources = autoreleasepool(|_| {
            let content = shareable_content().expect("shareable content");
            // SAFETY: Read-only snapshot; only owned Rust selectors leave scope.
            unsafe {
                let Some(display) = content.displays().firstObject() else {
                    eprintln!("SKIP native screen capture: no display");
                    return Vec::new();
                };
                let id = display.displayID();
                let mut sources = vec![SourceKey::Display(id), SourceKey::SystemAudio];
                if let Some(window) = content
                    .windows()
                    .iter()
                    .find(|w| w.isOnScreen() && w.owningApplication().is_some())
                {
                    sources.push(SourceKey::Window {
                        id: window.windowID(),
                        owner: window
                            .owningApplication()
                            .map(|app| app.bundleIdentifier().to_string()),
                    });
                } else {
                    eprintln!("SKIP window smoke: no shareable window");
                }
                // An explicit app makes smoke runs repeatable; otherwise choose
                // a shareable, unambiguous external app visible on this display.
                let bundle = std::env::var("OCULI_NATIVE_CAPTURE_APP").ok().or_else(|| {
                    let bounds = display.frame();
                    content.windows().iter().find_map(|window| {
                        let frame = window.frame();
                        if !window.isOnScreen()
                            || frame.origin.x >= bounds.origin.x + bounds.size.width
                            || bounds.origin.x >= frame.origin.x + frame.size.width
                            || frame.origin.y >= bounds.origin.y + bounds.size.height
                            || bounds.origin.y >= frame.origin.y + frame.size.height
                        {
                            return None;
                        }
                        let bundle = window.owningApplication()?.bundleIdentifier().to_string();
                        application(&content, &bundle).ok().map(|_| bundle)
                    })
                });
                if let Some(bundle) = bundle {
                    application(&content, &bundle)
                        .expect("unambiguous shareable smoke application");
                    sources.push(SourceKey::ApplicationVideo {
                        bundle: bundle.clone(),
                        display: id,
                    });
                    sources.push(SourceKey::ApplicationAudio(bundle));
                } else {
                    eprintln!(
                        "SKIP app video/audio smoke: no unambiguous visible external app; optionally set OCULI_NATIVE_CAPTURE_APP"
                    );
                }
                sources
            }
        });
        for source in sources {
            eprintln!("Native capture smoke: {source:?}");
            let kind = if source.is_audio() {
                SampleKind::Audio
            } else {
                SampleKind::Video
            };
            let format = match kind {
                SampleKind::Audio => CaptureFormat::Audio {
                    sample_rate: 48_000,
                    channels: 2,
                },
                SampleKind::Video => CaptureFormat::Video {
                    width: 320,
                    height: 180,
                    fps: 15,
                },
            };
            let mut registry = CaptureRegistry::new(Backend);
            let lease = registry
                .subscribe(
                    CaptureRequest {
                        source: source.clone(),
                        format,
                    },
                    if kind == SampleKind::Audio {
                        SubscriptionPolicy::Bounded {
                            capacity: 256,
                            audio: true,
                        }
                    } else {
                        SubscriptionPolicy::Latest
                    },
                )
                .unwrap_or_else(|error| panic!("{source:?}: {error}"));
            let deadline = Instant::now() + TIMEOUT;
            let mut timestamps = Vec::new();
            while Instant::now() < deadline && timestamps.len() < 2 {
                assert!(lease.error().is_none(), "{source:?}: {:?}", lease.error());
                if let Some(sample) = lease.try_recv() {
                    assert_eq!(sample.kind(), kind);
                    assert!(sample.timestamp_seconds().is_finite());
                    timestamps.push(sample.timestamp_seconds());
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            registry.shutdown().expect("native stream stops cleanly");
            assert_eq!(registry.session_count(), 0);
            assert_eq!(
                timestamps.len(),
                2,
                "{source:?}: animate the video source / play external audio during smoke tests"
            );
            assert!(
                timestamps[1] > timestamps[0],
                "{source:?}: advancing timestamps"
            );
        }
    }
}
