//! Serialized native capture owner. AppKit exchanges only Rust demand/status values.

use std::collections::HashMap;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

pub(crate) use super::preview::PreviewBridge;
pub use super::preview::PreviewConfig;
use super::preview::PreviewInput;
use super::{device_capture, sample::NativeSample, screen_capture};
use crate::capture::registry::SamplePublisher;
use crate::capture::service::{CaptureService, CaptureSnapshot, CaptureTarget};
use crate::capture::{CaptureBackend, CaptureError, CaptureRequest, CaptureSession, SourceKey};
use crate::model::{GroupCommand, RecordingConfig, RecordingId};
use crate::recording::{coordinator::GroupSnapshot, runtime::RecordingRuntime};
use crate::render::RenderedFrame;

pub struct NativeBackend;

// Process-lifetime quarantine also survives a capture worker restart after a
// cancelled application quit. Never open a second stream while native teardown
// of the first might still be pending. Only Rust values enter this shared map.
fn quarantine() -> &'static Mutex<HashMap<SourceKey, String>> {
    static SOURCES: OnceLock<Mutex<HashMap<SourceKey, String>>> = OnceLock::new();
    SOURCES.get_or_init(Default::default)
}

fn quarantine_source(source: &SourceKey, error: &CaptureError) {
    quarantine()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(source.clone(), error.to_string());
}

fn ensure_not_quarantined(source: &SourceKey) -> Result<(), CaptureError> {
    if let Some(reason) = quarantine()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(source)
    {
        Err(CaptureError::Unavailable(format!(
            "Native cleanup is uncertain ({reason}). Restart Oculi before retrying this source."
        )))
    } else {
        Ok(())
    }
}

struct GuardedSession {
    source: SourceKey,
    inner: Box<dyn CaptureSession>,
    stopped: Option<Result<(), CaptureError>>,
}

impl CaptureSession for GuardedSession {
    fn stop(&mut self) -> Result<(), CaptureError> {
        if let Some(result) = &self.stopped {
            return result.clone();
        }
        let result = self.inner.stop();
        if let Err(error) = &result {
            quarantine_source(&self.source, error);
        }
        self.stopped = Some(result.clone());
        result
    }
}

impl Drop for GuardedSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

impl CaptureBackend<NativeSample> for NativeBackend {
    fn start(
        &mut self,
        request: &CaptureRequest,
        publisher: SamplePublisher<NativeSample>,
    ) -> Result<Box<dyn CaptureSession>, CaptureError> {
        ensure_not_quarantined(&request.source)?;
        let result = match request.source {
            SourceKey::Camera(_) | SourceKey::Microphone(_) => {
                device_capture::start(request, publisher)
            }
            _ => screen_capture::start(request, publisher),
        };
        match result {
            Ok(inner) => Ok(Box::new(GuardedSession {
                source: request.source.clone(),
                inner,
                stopped: None,
            })),
            Err(error) => {
                if matches!(error, CaptureError::Timeout(_))
                    || (!matches!(
                        request.source,
                        SourceKey::Camera(_) | SourceKey::Microphone(_)
                    ) && matches!(error, CaptureError::Native(_)))
                {
                    quarantine_source(&request.source, &error);
                }
                Err(error)
            }
        }
    }
}

struct Demand {
    generation: u64,
    targets: Vec<CaptureTarget>,
    retry: bool,
}

pub struct CaptureWorker {
    previews: PreviewBridge,
    recording: RecordingRuntime,
    demand: Arc<Mutex<Option<Demand>>>,
    snapshot: Arc<Mutex<Option<CaptureSnapshot>>>,
    stop: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    last_targets: Option<Vec<CaptureTarget>>,
    generation: u64,
    retry: bool,
}

impl Default for CaptureWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureWorker {
    pub fn new() -> Self {
        let demand = Arc::new(Mutex::new(None::<Demand>));
        let snapshot = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let render_stop = Arc::new(AtomicBool::new(false));
        let previews = PreviewBridge::new(render_stop.clone());
        let recording = RecordingRuntime::new(previews.clone(), stop.clone());
        let runtime = recording.clone();
        let preview_inputs = previews.inputs.clone();
        let (input, output, stopping, done) = (
            demand.clone(),
            snapshot.clone(),
            stop.clone(),
            finished.clone(),
        );
        std::thread::spawn(move || {
            // A panic discards all native worker state and clears the immutable
            // handoff below; no partially updated pipeline is ever resumed.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut service = CaptureService::new(NativeBackend);
                struct FailureGuard<'a> {
                    runtime: &'a RecordingRuntime,
                    inputs: &'a arc_swap::ArcSwap<HashMap<RecordingId, PreviewInput>>,
                }
                impl Drop for FailureGuard<'_> {
                    fn drop(&mut self) {
                        if std::thread::panicking() {
                            // Fail and clear before native service teardown can
                            // wait on delegates while unwinding this worker.
                            self.runtime
                                .capture_failed("Capture worker panicked; group stopped".into());
                            self.inputs.store(Arc::new(HashMap::new()));
                        }
                    }
                }
                let _failure_guard = FailureGuard {
                    runtime: &runtime,
                    inputs: &preview_inputs,
                };
                let mut latest = HashMap::<RecordingId, PreviewInput>::new();
                let mut revision = 0_u64;
                let mut applied_targets = Vec::new();
                let mut deferred = None;
                let mut display_generation = 0;
                while !(stopping.load(Ordering::Acquire) && runtime.is_finished()) {
                    // Release all command locks before entering Apple frameworks.
                    let next = input.lock().unwrap().take();
                    objc2::rc::autoreleasepool(|_| {
                        let routing = runtime.routing();
                        if let Some(demand) = next {
                            display_generation = demand.generation;
                            if let Some(routing) = &routing
                                && !routing.accepts(&demand.targets)
                            {
                                routing.fail("A required recording source/output was removed or changed; group stopped".into());
                            }
                            // Freeze all capture demand until group finalization;
                            // catalog refresh/retry must never reopen active media.
                            deferred = Some(demand);
                        }
                        if routing.is_none()
                            && !stopping.load(Ordering::Acquire)
                            && let Some(Demand {
                                generation,
                                targets,
                                retry,
                            }) = deferred.take()
                        {
                            applied_targets = targets.clone();
                            service.reconcile(generation, targets, retry);
                        }
                        let mut status = service.poll_with(|target, sample| {
                            if sample.kind() == super::sample::SampleKind::Video {
                                revision = revision.wrapping_add(1);
                                latest.insert(
                                    target.recording,
                                    PreviewInput {
                                        source: target.request.source.clone(),
                                        sample,
                                        revision,
                                    },
                                );
                            } else if let Some(routing) = &routing {
                                routing.audio(target, sample);
                            }
                        });
                        latest.retain(|id, input| {
                            status.statuses.iter().any(|status| {
                                status.recording == *id
                                    && status.source == input.source
                                    && matches!(
                                        status.state,
                                        crate::capture::service::CaptureState::Receiving { .. }
                                    )
                            })
                        });
                        // This immutable internal status is not consumed by UI poll.
                        runtime.capture_status(
                            status.clone(),
                            applied_targets.clone(),
                            routing.as_deref(),
                        );
                        preview_inputs.store(Arc::new(latest.clone()));
                        status.generation = display_generation;
                        *output.lock().unwrap() = Some(status);
                    });
                    std::thread::sleep(Duration::from_millis(10));
                }
                *output.lock().unwrap() = Some(objc2::rc::autoreleasepool(|_| service.shutdown()));
            }));
            stopping.store(true, Ordering::Release);
            if result.is_err() {
                runtime.capture_failed("Capture worker stopped unexpectedly; group stopped".into());
                *output.lock().unwrap_or_else(|p| p.into_inner()) = Some(CaptureSnapshot {
                    shutdown_errors: vec![
                        "Capture worker stopped unexpectedly. Restart Oculi.".into(),
                    ],
                    ..Default::default()
                });
            }
            while !runtime.is_finished() {
                std::thread::sleep(Duration::from_millis(10));
            }
            preview_inputs.store(Arc::new(HashMap::new()));
            render_stop.store(true, Ordering::Release);
            done.store(true, Ordering::Release);
        });
        Self {
            previews,
            recording,
            demand,
            snapshot,
            stop,
            finished,
            last_targets: None,
            generation: 0,
            retry: false,
        }
    }

    /// Latest-demand mailbox. A busy worker cannot block AppKit or accumulate edits.
    pub fn update(&mut self, targets: Vec<CaptureTarget>) {
        if self.stop.load(Ordering::Acquire)
            || (self.last_targets.as_ref() == Some(&targets) && !self.retry)
        {
            return;
        }
        if let Ok(mut pending) = self.demand.try_lock() {
            self.generation += 1;
            // Preserve an explicit retry if another edit replaces a queued demand.
            let retry = self.retry || pending.as_ref().is_some_and(|p| p.retry);
            *pending = Some(Demand {
                generation: self.generation,
                targets: targets.clone(),
                retry,
            });
            self.last_targets = Some(targets);
            self.retry = false;
        }
    }

    pub fn retry(&mut self) {
        self.retry = true;
    }

    pub fn configure_previews(&self, configs: Vec<PreviewConfig>) {
        self.previews.configure(configs);
    }

    pub fn preview(&self, recording: RecordingId) -> Option<Result<RenderedFrame, String>> {
        self.previews.frame(recording)
    }

    /// Bounded nonblocking enqueue. Inspect group_snapshot for preparation,
    /// command rejection, per-recording save results and terminal errors.
    pub fn group_command(
        &self,
        command: GroupCommand,
        configs: Vec<RecordingConfig>,
    ) -> Result<u64, String> {
        self.recording.command(command, configs)
    }

    pub fn group_snapshot(&self) -> GroupSnapshot {
        self.recording.snapshot()
    }

    pub fn poll(&self) -> Option<CaptureSnapshot> {
        let result = self.snapshot.try_lock().ok()?.take()?;
        (result.generation == self.generation || !result.shutdown_errors.is_empty())
            .then_some(result)
    }

    pub fn shutdown(&self) {
        // The runtime finishes committed media first. Capture and preview keep
        // running through the audio drain boundary and asynchronous MP4 finish.
        self.stop.store(true, Ordering::Release);
    }
    pub fn is_stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
            && self.previews.is_finished()
            && self.recording.is_finished()
    }
}

impl Drop for CaptureWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Explicit app-identity smoke check. No permission requests or saved settings
/// changes; successful MP4s remain in a unique directory under target/smoke.
#[allow(deprecated)]
pub fn recording_smoke_check() -> Result<String, String> {
    use crate::capture::{CaptureFormat, service::CaptureState};
    use crate::model::{AudioSourceSelector, GroupState, Resolution, SourceAvailability};
    use std::time::Instant;

    if !super::permissions::screen_granted() {
        return Ok("SKIP native recording: existing Screen & System Audio permission required; NOT verified. No permissions requested.".into());
    }
    let catalog = super::discovery::refresh()
        .recv_timeout(Duration::from_secs(10))
        .map_err(|error| format!("Recording source discovery: {error}"))?;
    let Some(video) = catalog
        .videos
        .iter()
        .find(|video| matches!(SourceKey::video(video), Some(SourceKey::Display(_))))
        .cloned()
    else {
        return Ok("SKIP native recording: authorized display unavailable; NOT verified".into());
    };
    let source = SourceKey::video(&video).unwrap();
    let directory = std::env::current_dir()
        .map_err(|e| e.to_string())?
        .join("target/smoke")
        .join(format!("recording-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    let mut configs = (0..2)
        .map(|i| {
            let mut config = RecordingConfig::new(format!("smoke-{}", i + 1));
            config.video = video.clone();
            config.audio = AudioSourceSelector::System;
            config.video_availability = SourceAvailability::Available;
            config.audio_availability = SourceAvailability::Available;
            config.output.resolution = Resolution {
                width: 320,
                height: 180,
            };
            config.output.fps = 30;
            config.output.directory = directory.clone();
            config
        })
        .collect::<Vec<_>>();
    configs[1].viewport.crop_width = 0.5;
    let mut previews = configs
        .iter()
        .enumerate()
        .map(|(i, config)| PreviewConfig {
            recording: config.id,
            source: source.clone(),
            transform: config.viewport,
            canvas: config.output.resolution,
            fps: config.output.fps,
            active: i == 0,
            visible: i == 0,
        })
        .collect::<Vec<_>>();
    let targets = configs
        .iter()
        .flat_map(|config| {
            [
                CaptureTarget {
                    recording: config.id,
                    request: CaptureRequest {
                        source: source.clone(),
                        format: CaptureFormat::Video {
                            width: 320,
                            height: 180,
                            fps: 30,
                        },
                    },
                },
                CaptureTarget {
                    recording: config.id,
                    request: CaptureRequest {
                        source: SourceKey::SystemAudio,
                        format: CaptureFormat::Audio {
                            sample_rate: 48_000,
                            channels: 2,
                        },
                    },
                },
            ]
        })
        .collect();
    let mut worker = CaptureWorker::new();
    worker.configure_previews(previews.clone());
    worker.update(targets);
    let result = (|| {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            if let Some(status) = worker.poll() {
                if let Some(error) = status.statuses.iter().find_map(|s| {
                    if let CaptureState::Failed(error) = &s.state {
                        Some(error.to_string())
                    } else {
                        None
                    }
                }) {
                    return Err(error);
                }
                if status.statuses.len() == 4
                    && status
                        .statuses
                        .iter()
                        .all(|s| matches!(s.state, CaptureState::Receiving { .. }))
                {
                    if status.active_sessions != 2 {
                        return Err(format!("Expected two shared capture sessions: {status:?}"));
                    }
                    break;
                }
            }
            if Instant::now() >= deadline {
                return Err("Timed out waiting for display/system audio".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let wait = |state: GroupState| -> Result<GroupSnapshot, String> {
            let deadline = Instant::now() + Duration::from_secs(40);
            loop {
                let snapshot = worker.group_snapshot();
                if snapshot.state == GroupState::Failed {
                    return Err(format!("Recording failed: {snapshot:?}"));
                }
                if snapshot.state == state {
                    return Ok(snapshot);
                }
                if Instant::now() >= deadline {
                    return Err(format!("Timed out waiting for {state:?}: {snapshot:?}"));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        worker.group_command(GroupCommand::Start, configs.clone())?;
        wait(GroupState::Recording)?;
        std::thread::sleep(Duration::from_millis(650));
        worker.group_command(GroupCommand::Pause, Vec::new())?;
        let paused = wait(GroupState::Paused)?.elapsed;
        std::thread::sleep(Duration::from_millis(200));
        if (worker.group_snapshot().elapsed - paused).abs() > 1e-6 {
            return Err("Paused duration advanced".into());
        }
        previews[1].transform.crop_x = 0.5;
        worker.configure_previews(previews.clone());
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            if let Some(Ok(frame)) = worker.previews.recording_frame(configs[1].id)
                && frame.transform == previews[1].transform
            {
                break;
            }
            if Instant::now() >= deadline {
                return Err("Hidden recording did not apply live crop".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        worker.group_command(GroupCommand::Resume, Vec::new())?;
        wait(GroupState::Recording)?;
        std::thread::sleep(Duration::from_millis(650));
        worker.group_command(GroupCommand::Stop, Vec::new())?;
        let saved = wait(GroupState::Idle)?;
        if saved.recordings.len() != 2
            || saved
                .recordings
                .iter()
                .any(|r| r.saved_path.is_none() || r.error.is_some())
        {
            return Err(format!("Missing saved recording results: {saved:?}"));
        }
        let mut paths = Vec::new();
        for report in &saved.recordings {
            let path = report.saved_path.as_ref().unwrap();
            // SAFETY: Read-only file asset and track metadata, worker-local.
            unsafe {
                use objc2_av_foundation::{AVMediaTypeAudio, AVMediaTypeVideo, AVURLAsset};
                use objc2_foundation::{NSString, NSURL};
                let url = NSURL::fileURLWithPath(&NSString::from_str(
                    path.to_str().ok_or("Non-UTF8 smoke path")?,
                ));
                let asset = AVURLAsset::URLAssetWithURL_options(&url, None);
                let videos = asset
                    .tracksWithMediaType(AVMediaTypeVideo.ok_or("Video media type unavailable")?);
                let audios = asset
                    .tracksWithMediaType(AVMediaTypeAudio.ok_or("Audio media type unavailable")?);
                if videos.count() != 1 || audios.count() != 1 {
                    return Err("Saved MP4 must contain video and audio".into());
                }
                let size = videos.objectAtIndex(0).naturalSize();
                let duration = asset.duration();
                let seconds = duration.value as f64 / f64::from(duration.timescale);
                if size.width != 320.0
                    || size.height != 180.0
                    || !seconds.is_finite()
                    || (seconds - saved.elapsed).abs() > 0.15
                {
                    return Err(format!(
                        "Unexpected MP4 dimensions/duration: {size:?}, {seconds:.3}s vs {:.3}s",
                        saved.elapsed
                    ));
                }
            }
            paths.push(path.display().to_string());
        }
        Ok(format!(
            "PASS two native H.264/AAC MP4s, 320x180/30, shared display/system-audio sessions, pause/resume, independent live crop (including hidden output), {:.3}s active duration. Saved: {}",
            saved.elapsed,
            paths.join(", ")
        ))
    })();
    worker.shutdown();
    let deadline = Instant::now() + Duration::from_secs(45);
    while !worker.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if !worker.is_finished() {
        return Err(format!(
            "Recording/capture shutdown timed out; artifacts: {}",
            directory.display()
        ));
    }
    if let Some(status) = worker.poll()
        && (status.active_sessions != 0 || !status.shutdown_errors.is_empty())
    {
        return Err(format!("Recording shutdown failed: {status:?}"));
    }
    result.map(|message| {
        format!("{message}; all workers stopped cleanly. No saved settings changed.")
    })
}

/// Explicit, permission-gated end-to-end check with isolated, unsaved configurations.
pub fn preview_smoke_check() -> Result<String, String> {
    use crate::capture::{CaptureFormat, service::CaptureState};
    use crate::model::{Resolution, ViewportTransform};
    use std::time::Instant;

    let camera = std::env::var("OCULI_PREVIEW_SOURCE").as_deref() == Ok("camera");
    let catalog = super::discovery::refresh()
        .recv_timeout(Duration::from_secs(10))
        .map_err(|error| format!("Preview source discovery: {error}"))?;
    let source = catalog
        .videos
        .iter()
        .filter_map(SourceKey::video)
        .find(|source| {
            if camera {
                matches!(source, SourceKey::Camera(_))
            } else {
                matches!(source, SourceKey::Display(_))
            }
        });
    let Some(source) = source else {
        return Ok(format!(
            "SKIP {} preview: authorized source unavailable; NOT verified. No permissions requested.",
            if camera { "camera" } else { "display" }
        ));
    };
    let mut worker = CaptureWorker::new();
    let ids = [RecordingId::new(), RecordingId::new()];
    let targets = ids
        .iter()
        .map(|recording| CaptureTarget {
            recording: *recording,
            request: CaptureRequest {
                source: source.clone(),
                format: CaptureFormat::Video {
                    width: 640,
                    height: 360,
                    fps: 15,
                },
            },
        })
        .collect::<Vec<_>>();
    let mut configs = ids
        .iter()
        .map(|recording| PreviewConfig {
            recording: *recording,
            source: source.clone(),
            transform: ViewportTransform::default(),
            canvas: Resolution {
                width: 640,
                height: 360,
            },
            fps: 15,
            active: true,
            visible: true,
        })
        .collect::<Vec<_>>();
    worker.configure_previews(configs.clone());
    worker.update(targets.clone());
    let result = (|| {
        let wait = |expected: &[PreviewConfig]| -> Result<(), String> {
            let deadline = Instant::now() + Duration::from_secs(12);
            loop {
                if let Some(snapshot) = worker.poll() {
                    if let Some(error) =
                        snapshot
                            .statuses
                            .iter()
                            .find_map(|status| match &status.state {
                                CaptureState::Failed(error) => Some(error.to_string()),
                                _ => None,
                            })
                    {
                        return Err(error);
                    }
                    if snapshot.active_sessions != 1 || !snapshot.shutdown_errors.is_empty() {
                        return Err(format!("Expected one shared source: {snapshot:?}"));
                    }
                }
                let mut ready = 0;
                for config in expected {
                    match worker.preview(config.recording) {
                        Some(Ok(frame))
                            if frame.transform == config.transform
                                && frame.canvas == config.canvas =>
                        {
                            if !frame.timestamp_seconds.is_finite()
                                || frame.source_size.width == 0
                                || frame.source_size.height == 0
                            {
                                return Err("Invalid rendered frame metadata".into());
                            }
                            ready += 1;
                        }
                        Some(Err(error)) => return Err(error),
                        _ => {}
                    }
                }
                if ready == expected.len() {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err("Timed out waiting for live preview transforms".into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait(&configs)?;
        configs[1].transform.scale = 1.75;
        configs[1].transform.crop_x = 0.1;
        configs[1].transform.crop_width = 0.8;
        configs[1].transform.offset_y = 0.1;
        worker.configure_previews(configs.clone());
        wait(&configs)?;
        configs[1].canvas = Resolution {
            width: 320,
            height: 240,
        };
        worker.configure_previews(configs.clone());
        wait(&configs)?;
        // A transform-only change must not touch the capture demand/generation.
        if worker.generation != 1 {
            return Err("Framing restarted capture".into());
        }
        Ok(format!(
            "PASS {source:?}: two live GPU previews, independent crop/zoom/placement and output resizing, one shared capture session, transform edits without capture restart"
        ))
    })();
    worker.shutdown();
    let deadline = Instant::now() + Duration::from_secs(12);
    while !worker.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if !worker.is_finished() {
        return Err("Preview/capture shutdown timed out".into());
    }
    if let Some(snapshot) = worker.poll()
        && (snapshot.active_sessions != 0 || !snapshot.shutdown_errors.is_empty())
    {
        return Err(format!("Preview shutdown failed: {snapshot:?}"));
    }
    result.map(|message| {
        format!("{message}; capture and render workers stopped cleanly. No media files written.")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mailbox() -> CaptureWorker {
        let stop = Arc::new(AtomicBool::new(false));
        let previews = PreviewBridge::new(stop.clone());
        let recording = RecordingRuntime::new(previews.clone(), stop.clone());
        CaptureWorker {
            previews,
            recording,
            demand: Arc::new(Mutex::new(None)),
            snapshot: Arc::new(Mutex::new(None)),
            stop,
            finished: Arc::new(AtomicBool::new(false)),
            last_targets: None,
            generation: 0,
            retry: false,
        }
    }

    #[test]
    fn pending_demand_is_replaced_without_losing_retry() {
        let mut worker = mailbox();
        worker.retry();
        worker.update(Vec::new());
        worker.last_targets = None;
        worker.update(Vec::new());
        let pending = worker.demand.lock().unwrap();
        assert_eq!(pending.as_ref().unwrap().generation, 2);
        assert!(pending.as_ref().unwrap().retry);
    }

    #[test]
    fn busy_mailboxes_never_block_or_mark_unsent_demand_as_sent() {
        let mut worker = mailbox();
        let demand = worker.demand.clone();
        let guard = demand.lock().unwrap();
        worker.update(Vec::new());
        assert!(worker.last_targets.is_none());
        assert_eq!(worker.generation, 0);
        drop(guard);
        worker.update(Vec::new());
        assert_eq!(worker.generation, 1);
        let snapshot = worker.snapshot.clone();
        let _guard = snapshot.lock().unwrap();
        assert!(worker.poll().is_none());
    }

    #[test]
    fn obsolete_snapshots_are_ignored_and_shutdown_rejects_new_demand() {
        let mut worker = mailbox();
        worker.update(Vec::new());
        *worker.snapshot.lock().unwrap() = Some(CaptureSnapshot::default());
        assert!(worker.poll().is_none());
        *worker.snapshot.lock().unwrap() = Some(CaptureSnapshot {
            generation: 1,
            ..Default::default()
        });
        assert_eq!(worker.poll().unwrap().generation, 1);
        worker.shutdown();
        worker.retry();
        worker.update(Vec::new());
        assert_eq!(worker.generation, 1);
    }

    #[test]
    fn uncertain_stop_quarantines_source_across_backend_recreation() {
        struct FailedStop;
        impl CaptureSession for FailedStop {
            fn stop(&mut self) -> Result<(), CaptureError> {
                Err(CaptureError::Timeout("test stop".into()))
            }
        }
        let source = SourceKey::ApplicationAudio(format!("test.{}", uuid::Uuid::new_v4()));
        assert!(ensure_not_quarantined(&source).is_ok());
        let mut session = GuardedSession {
            source: source.clone(),
            inner: Box::new(FailedStop),
            stopped: None,
        };
        assert!(session.stop().is_err());
        drop(session);
        let error = ensure_not_quarantined(&source).unwrap_err().to_string();
        assert!(error.contains("Restart Oculi"));
        assert!(
            ensure_not_quarantined(&SourceKey::ApplicationAudio(format!(
                "test.{}",
                uuid::Uuid::new_v4()
            )))
            .is_ok()
        );
    }

    #[test]
    fn native_shared_preview_smoke() {
        if std::env::var("OCULI_NATIVE_CAPTURE_SMOKE").as_deref() != Ok("1") {
            eprintln!("SKIP shared GPU preview capture: explicit opt-in required; NOT verified");
            return;
        }
        let report = preview_smoke_check().expect("shared GPU previews");
        eprintln!("{report}");
    }

    #[test]
    fn native_recording_group_smoke() {
        if std::env::var("OCULI_NATIVE_CAPTURE_SMOKE").as_deref() != Ok("1") {
            eprintln!("SKIP native recording group: explicit opt-in required; NOT verified");
            return;
        }
        eprintln!(
            "{}",
            recording_smoke_check().expect("native recording group")
        );
    }

    #[test]
    fn native_shared_capture_smoke() {
        if std::env::var("OCULI_NATIVE_CAPTURE_SMOKE").as_deref() != Ok("1")
            || !super::super::permissions::screen_granted()
        {
            eprintln!(
                "SKIP shared native capture: opt-in and existing screen permission required; never prompting"
            );
            return;
        }
        let catalog = super::super::discovery::refresh()
            .recv_timeout(Duration::from_secs(10))
            .expect("source discovery");
        let Some(source) = catalog
            .videos
            .iter()
            .filter_map(SourceKey::video)
            .find(|s| matches!(s, SourceKey::Display(_)))
        else {
            eprintln!("SKIP shared native capture: no available display");
            return;
        };
        objc2::rc::autoreleasepool(|_| {
            use crate::capture::CaptureFormat;
            use crate::capture::service::CaptureState;
            let mut service = CaptureService::new(NativeBackend);
            let mut targets = Vec::new();
            for _ in 0..2 {
                let recording = crate::model::RecordingId::new();
                targets.push(CaptureTarget {
                    recording,
                    request: CaptureRequest {
                        source: source.clone(),
                        format: CaptureFormat::Video {
                            width: 640,
                            height: 360,
                            fps: 15,
                        },
                    },
                });
                targets.push(CaptureTarget {
                    recording,
                    request: CaptureRequest {
                        source: SourceKey::SystemAudio,
                        format: CaptureFormat::Audio {
                            sample_rate: 48_000,
                            channels: 2,
                        },
                    },
                });
            }
            service.reconcile(1, targets.clone(), false);
            let deadline = std::time::Instant::now() + Duration::from_secs(8);
            loop {
                let snapshot = service.poll();
                assert!(
                    snapshot
                        .statuses
                        .iter()
                        .all(|s| !matches!(s.state, CaptureState::Failed(_))),
                    "{snapshot:?}"
                );
                if snapshot
                    .statuses
                    .iter()
                    .all(|s| matches!(s.state, CaptureState::Receiving { .. }))
                {
                    assert_eq!(snapshot.statuses.len(), 4);
                    assert_eq!(
                        snapshot.active_sessions, 2,
                        "duplicate consumers must share native streams"
                    );
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "capture sample timeout: {snapshot:?}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            targets.drain(0..2);
            service.reconcile(2, targets, false);
            assert_eq!(
                service.poll().active_sessions,
                2,
                "removing one pair must not stop the other"
            );
            let stopped = service.shutdown();
            assert_eq!(stopped.active_sessions, 0);
            assert!(stopped.shutdown_errors.is_empty(), "{stopped:?}");
            eprintln!(
                "PASS shared native capture: two video/audio pairs, two native sessions, clean teardown"
            );
        });
    }
}
