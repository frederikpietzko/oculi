//! Serialized native group orchestration. Capture only publishes bounded Rust
//! handoffs; this worker owns reservations, timeline routing and encoder control.

pub use crate::platform::macos::capture::recording_smoke_check;

use super::{
    clock::host_seconds,
    coordinator::{Coordinator, GroupBackend, GroupSnapshot, RecordingReport},
    encoder::{Encoder, EncoderState},
    timeline::{FrameCadence, Timeline},
};
use crate::{
    capture::{
        CaptureFormat, SourceKey,
        service::{CaptureSnapshot, CaptureState, CaptureTarget, negotiate},
    },
    model::{GroupCommand, GroupState, RecordingConfig, RecordingId, SourceAvailability},
    platform::macos::{
        capture::{PreviewBridge, PreviewConfig},
        sample::{NativeSample, SampleKind},
    },
    storage::{naming::TemplateContext, reservation::OutputReservation},
};
use arc_swap::{ArcSwap, ArcSwapOption};
use crossbeam_queue::ArrayQueue;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
    },
    time::{Duration, Instant},
};

const AUDIO_CAPACITY: usize = 512;
const DRAIN_GRACE: Duration = Duration::from_millis(200);

struct AudioPacket {
    recording: RecordingId,
    sample: NativeSample,
}

pub(crate) struct Routing {
    epoch: u64,
    targets: Vec<CaptureTarget>,
    audio: ArrayQueue<AudioPacket>,
    depths: HashMap<RecordingId, AtomicUsize>,
    error: OnceLock<String>,
    accepting: AtomicBool,
    publishing: AtomicUsize,
}

impl Routing {
    pub(crate) fn fail(&self, error: String) {
        let _ = self.error.set(error);
    }

    pub(crate) fn accepts(&self, targets: &[CaptureTarget]) -> bool {
        self.targets
            .iter()
            .all(|required| targets.contains(required))
    }

    /// Called only by capture's poll observer. Overflow is latched for the whole
    /// group, not retried or silently converted to a missing audio segment.
    pub(crate) fn audio(&self, target: &CaptureTarget, sample: NativeSample) {
        self.publishing.fetch_add(1, Ordering::SeqCst);
        if self.accepting.load(Ordering::SeqCst)
            && self.targets.contains(target)
            && let Some(depth) = self.depths.get(&target.recording)
        {
            depth.fetch_add(1, Ordering::Relaxed);
            if self
                .audio
                .push(AudioPacket {
                    recording: target.recording,
                    sample,
                })
                .is_err()
            {
                depth.fetch_sub(1, Ordering::Relaxed);
                self.fail("Capture-to-recording audio queue overflow; group stopped".into());
            }
        }
        self.publishing.fetch_sub(1, Ordering::SeqCst);
    }

    fn pop_audio(&self) -> Option<AudioPacket> {
        let packet = self.audio.pop()?;
        self.depths[&packet.recording].fetch_sub(1, Ordering::Relaxed);
        Some(packet)
    }
}

#[derive(Default)]
struct CaptureView {
    status: CaptureSnapshot,
    targets: Vec<CaptureTarget>,
    epoch: Option<u64>,
    host: f64,
}

struct Shared {
    capture: ArcSwap<CaptureView>,
    routing: ArcSwapOption<Routing>,
    snapshot: ArcSwap<GroupSnapshot>,
    finished: AtomicBool,
}

struct Command {
    sequence: u64,
    command: GroupCommand,
    configs: Vec<RecordingConfig>,
}

struct Commands {
    sender: SyncSender<Command>,
    next: u64,
}

#[derive(Clone)]
pub(crate) struct RecordingRuntime {
    shared: Arc<Shared>,
    commands: Arc<Mutex<Commands>>,
    shutdown: Arc<AtomicBool>,
}

impl RecordingRuntime {
    pub(crate) fn new(previews: PreviewBridge, shutdown: Arc<AtomicBool>) -> Self {
        let shared = Arc::new(Shared {
            capture: ArcSwap::from_pointee(CaptureView::default()),
            routing: ArcSwapOption::empty(),
            snapshot: ArcSwap::from_pointee(GroupSnapshot::default()),
            finished: AtomicBool::new(false),
        });
        let (commands, receiver) = mpsc::sync_channel::<Command>(16);
        let worker = shared.clone();
        let stopping = shutdown.clone();
        std::thread::spawn(move || {
            let mut coordinator =
                Coordinator::new(NativeGroupBackend::new(worker.clone(), previews));
            let mut processed_sequence = 0;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                loop {
                    objc2::rc::autoreleasepool(|_| {
                        for _ in 0..16 {
                            let Ok(command) = receiver.try_recv() else {
                                break;
                            };
                            let result = if stopping.load(Ordering::Acquire) {
                                Err(
                                    "Command rejected: capture/recording worker is shutting down"
                                        .into(),
                                )
                            } else {
                                coordinator.command(
                                    command.command,
                                    command.configs,
                                    host_seconds(),
                                )
                            };
                            if let Err(error) = result {
                                // Rejected commands are visible but do not fail a healthy group.
                                coordinator.backend_mut().command_error = Some(error);
                            } else {
                                coordinator.backend_mut().command_error = None;
                            }
                            // Publish only after prepare/rollback or rejection has
                            // completed, never on dequeue or optimistic UI enqueue.
                            processed_sequence = command.sequence;
                        }
                        let host = host_seconds();
                        if stopping.load(Ordering::Acquire) && coordinator.is_active() {
                            let _ = coordinator.command(GroupCommand::Stop, Vec::new(), host);
                        }
                        coordinator.backend_mut().pump(host);
                        coordinator.tick(host);
                        coordinator.backend_mut().pump(host);
                        let mut snapshot = coordinator.snapshot(host);
                        snapshot.command_sequence = processed_sequence;
                        if snapshot.error.is_none() {
                            snapshot.error = coordinator.backend_mut().command_error.clone();
                        }
                        worker.snapshot.store(Arc::new(snapshot));
                    });
                    if stopping.load(Ordering::Acquire) && !coordinator.is_active() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(4));
                }
            }));
            if result.is_err() {
                stopping.store(true, Ordering::Release);
                // Keep encoder handles alive while requesting asynchronous finish,
                // even on panic. Drop must not turn committed media into rollback.
                let host = host_seconds();
                let backend = coordinator.backend_mut();
                backend.fail("Recording runtime panicked; group stopped".into());
                if let Some(timeline) = backend.timeline.as_mut() {
                    let _ = timeline.stop(host);
                    let end = timeline.elapsed(host);
                    backend.begin_finish(end);
                } else {
                    backend.begin_rollback();
                }
                while !backend.finalized() {
                    backend.pump(host_seconds());
                    std::thread::sleep(Duration::from_millis(10));
                }
                worker.snapshot.store(Arc::new(GroupSnapshot {
                    command_sequence: processed_sequence,
                    state: GroupState::Failed,
                    elapsed: backend.timeline.as_ref().map_or(0.0, |t| t.elapsed(host)),
                    recordings: backend.reports(),
                    error: backend.error(),
                }));
            }
            worker.routing.store(None);
            worker.finished.store(true, Ordering::Release);
        });
        Self {
            shared,
            commands: Arc::new(Mutex::new(Commands {
                sender: commands,
                next: 1,
            })),
            shutdown,
        }
    }

    /// Success means accepted into the bounded command queue, not prepared or
    /// saved. The immutable group snapshot is the authoritative completion result.
    pub(crate) fn command(
        &self,
        command: GroupCommand,
        configs: Vec<RecordingConfig>,
    ) -> Result<u64, String> {
        if self.shutdown.load(Ordering::Acquire) || self.is_finished() {
            return Err("Capture/recording worker is shutting down".into());
        }
        // Serialize sequence allocation with enqueue, including concurrent callers.
        // try_lock keeps the API nonblocking even if another producer is paused.
        let mut commands = self
            .commands
            .try_lock()
            .map_err(|_| "Recording command sender is busy; retry")?;
        let sequence = commands.next;
        let next = sequence
            .checked_add(1)
            .ok_or("Recording command sequence exhausted")?;
        commands
            .sender
            .try_send(Command {
                sequence,
                command,
                configs,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => "Recording command queue is full; retry".into(),
                mpsc::TrySendError::Disconnected(_) => String::from("Recording worker has stopped"),
            })?;
        commands.next = next;
        Ok(sequence)
    }

    pub(crate) fn snapshot(&self) -> GroupSnapshot {
        (**self.shared.snapshot.load()).clone()
    }
    pub(crate) fn is_finished(&self) -> bool {
        self.shared.finished.load(Ordering::Acquire)
    }
    pub(crate) fn routing(&self) -> Option<Arc<Routing>> {
        self.shared.routing.load_full()
    }

    pub(crate) fn capture_status(
        &self,
        status: CaptureSnapshot,
        targets: Vec<CaptureTarget>,
        routing: Option<&Routing>,
    ) {
        self.shared.capture.store(Arc::new(CaptureView {
            status,
            targets,
            epoch: routing.map(|r| r.epoch),
            host: host_seconds(),
        }));
    }

    pub(crate) fn capture_failed(&self, error: String) {
        if let Some(routing) = self.routing() {
            routing.fail(error);
        }
    }
}

struct Entry {
    config: RecordingConfig,
    encoder: Option<Encoder>,
    report: Option<RecordingReport>,
    cadence: FrameCadence,
    missing_video: u64,
    last_frame: Instant,
    last_audio: Instant,
    audio_level: f32,
}

struct NativeGroupBackend {
    shared: Arc<Shared>,
    previews: PreviewBridge,
    entries: Vec<Entry>,
    timeline: Option<Timeline>,
    routing: Option<Arc<Routing>>,
    pending: VecDeque<AudioPacket>,
    failure: Option<String>,
    command_error: Option<String>,
    preparing_since: Instant,
    finish: Option<(f64, Instant)>,
    closed: bool,
}

impl NativeGroupBackend {
    fn new(shared: Arc<Shared>, previews: PreviewBridge) -> Self {
        Self {
            shared,
            previews,
            entries: Vec::new(),
            timeline: None,
            routing: None,
            pending: VecDeque::new(),
            failure: None,
            command_error: None,
            preparing_since: Instant::now(),
            finish: None,
            closed: false,
        }
    }

    fn fail(&mut self, error: String) {
        if self.failure.is_none() {
            self.failure = Some(error);
        }
    }

    fn check_capture(&self, host: f64) -> Result<bool, String> {
        let Some(routing) = self.routing.as_ref() else {
            return Ok(false);
        };
        if let Some(error) = routing.error.get() {
            return Err(error.clone());
        }
        let view = self.shared.capture.load();
        if host - view.host > 2.0 {
            return Err("Capture status stopped updating".into());
        }
        validate_live(&routing.targets, &view)?;
        Ok(view.epoch == Some(routing.epoch))
    }

    fn pump(&mut self, host: f64) {
        if self.routing.is_none() || self.closed {
            return;
        }
        if self.finish.is_none() {
            if let Err(error) = self.check_capture(host) {
                self.fail(error);
            }
            if self.previews.is_finished() {
                self.fail("Render worker stopped".into());
            }
        }
        let routing = self.routing.as_ref().unwrap().clone();
        if let Some(error) = routing.error.get() {
            self.fail(error.clone());
        }
        let draining = self
            .finish
            .is_some_and(|(_, since)| since.elapsed() >= DRAIN_GRACE);
        if draining {
            // Close admission before draining. The in-flight counter covers the
            // producer's check/push race; never finish over an accepted packet.
            routing.accepting.store(false, Ordering::SeqCst);
        }
        for _ in 0..AUDIO_CAPACITY {
            let Some(packet) = routing.pop_audio() else {
                break;
            };
            if self.pending.len() >= AUDIO_CAPACITY {
                self.fail("Pending audio queue overflow; group stopped".into());
                break;
            }
            self.pending.push_back(packet);
        }
        let count = self.pending.len();
        for _ in 0..count {
            let packet = self.pending.pop_front().unwrap();
            let info = match audio_info(&packet.sample) {
                Ok(info) => info,
                Err(error) => {
                    self.fail(error);
                    continue;
                }
            };
            let start = packet.sample.timestamp_seconds();
            let end = start + info.frames as f64 / f64::from(info.rate);
            if end > host && self.finish.is_none() {
                if end - host > 0.25 {
                    self.fail("Audio clock is ahead of the host clock".into());
                } else {
                    self.pending.push_back(packet);
                }
                continue;
            }
            let Some(entry) = self
                .entries
                .iter_mut()
                .find(|e| e.config.id == packet.recording)
            else {
                continue;
            };
            entry.last_audio = Instant::now();
            entry.audio_level = info.level;
            let Some(timeline) = &self.timeline else {
                continue;
            };
            let result = timeline
                .audio_spans(start, info.frames, info.rate)
                .and_then(|spans| {
                    let encoder = entry.encoder.as_mut().ok_or("Missing prepared encoder")?;
                    for span in spans {
                        encoder.audio(
                            packet.sample.clone(),
                            span.offset,
                            span.frames,
                            span.timestamp,
                        )?;
                    }
                    Ok(())
                });
            if let Err(error) = result {
                self.fail(error);
            }
        }
        if self.failure.is_none()
            && self.finish.is_none()
            && let Some(timeline) = &self.timeline
        {
            let active = timeline.map(host).is_some();
            let elapsed = timeline.elapsed(host);
            let mut failure = None;
            for entry in &mut self.entries {
                if entry.last_audio.elapsed() > Duration::from_secs(2) {
                    failure = Some(format!(
                        "{}: audio source stopped delivering samples",
                        entry.config.name
                    ));
                    break;
                }
                if !active {
                    continue;
                }
                let frame = self.previews.recording_frame(entry.config.id);
                if let Some(Err(error)) = &frame
                    && !error.starts_with("Allocate bounded canvas buffer")
                {
                    failure = Some(error.clone());
                    break;
                }
                if let Some(Ok(_)) = &frame {
                    entry.last_frame = Instant::now();
                }
                if let Some(Ok(frame)) = &frame
                    && matches!(
                        entry.config.video,
                        crate::model::VideoSourceSelector::Camera { .. }
                    )
                    && host - frame.timestamp_seconds > 2.0
                {
                    failure = Some(format!(
                        "{}: camera video stopped delivering frames",
                        entry.config.name
                    ));
                    break;
                }
                if entry.last_frame.elapsed() > Duration::from_secs(2) {
                    failure = Some(format!(
                        "{}: rendered source unavailable",
                        entry.config.name
                    ));
                    break;
                }
                if let Some(timestamp) = entry.cadence.due(elapsed) {
                    if let Some(Ok(frame)) = frame {
                        entry.encoder.as_mut().unwrap().video(frame, timestamp);
                    } else {
                        entry.missing_video += 1;
                    }
                }
            }
            if let Some(error) = failure {
                self.fail(error);
            }
        }
        if let Some((end, _)) = self.finish
            && draining
            && routing.publishing.load(Ordering::SeqCst) == 0
            && routing.audio.is_empty()
            && self.pending.is_empty()
        {
            // The 200ms grace is the explicit input boundary. Everything already
            // queued was drained above using the stopped common timeline.
            for entry in &mut self.entries {
                if let Some(encoder) = &mut entry.encoder {
                    encoder.finish(end);
                }
            }
            self.closed = true;
        }
    }
}

impl GroupBackend for NativeGroupBackend {
    fn prepare(&mut self, configs: &[RecordingConfig]) -> Result<(), String> {
        self.entries.clear();
        self.timeline = None;
        self.routing = None;
        self.pending.clear();
        self.failure = None;
        self.finish = None;
        self.closed = false;
        self.preparing_since = Instant::now();
        let targets = preflight(configs, &self.shared.capture.load())?;
        static EPOCH: AtomicU64 = AtomicU64::new(1);
        let routing = Arc::new(Routing {
            epoch: EPOCH.fetch_add(1, Ordering::Relaxed),
            depths: configs
                .iter()
                .map(|config| (config.id, AtomicUsize::new(0)))
                .collect(),
            targets,
            audio: ArrayQueue::new(AUDIO_CAPACITY),
            error: OnceLock::new(),
            accepting: AtomicBool::new(true),
            publishing: AtomicUsize::new(0),
        });
        self.routing = Some(routing.clone());
        self.shared.routing.store(Some(routing));
        self.previews.record(
            configs
                .iter()
                .map(|config| PreviewConfig {
                    recording: config.id,
                    source: SourceKey::video(&config.video).unwrap(),
                    transform: config.viewport,
                    canvas: config.output.resolution,
                    fps: config.output.fps,
                    active: true,
                    visible: false,
                })
                .collect(),
        );
        self.entries = configs
            .iter()
            .map(|config| Entry {
                config: config.clone(),
                encoder: None,
                report: None,
                cadence: FrameCadence::new(config.output.fps).unwrap(),
                missing_video: 0,
                last_frame: Instant::now(),
                last_audio: Instant::now(),
                audio_level: 0.0,
            })
            .collect();
        // No native writer exists until every destination is reserved. All names
        // use one wall-clock instant; media timing always uses the host clock.
        let timestamp = time::OffsetDateTime::now_utc();
        let mut reservations = Vec::new();
        for config in configs {
            match OutputReservation::reserve(
                &config.output.directory,
                &config.output.filename_template,
                &TemplateContext {
                    name: config.name.clone(),
                    timestamp,
                    counter: 1,
                },
            ) {
                Ok(reservation) => reservations.push(reservation),
                Err(mut error) => {
                    for reservation in reservations {
                        if let Err(cleanup) = reservation.abort() {
                            error.push_str(&format!("; {cleanup}"));
                        }
                    }
                    self.fail(error.clone());
                    return Err(error);
                }
            }
        }
        for (entry, reservation) in self.entries.iter_mut().zip(reservations) {
            entry.encoder = Some(Encoder::prepare(entry.config.output.clone(), reservation));
        }
        Ok(())
    }

    fn prepared(&mut self) -> Result<bool, String> {
        if let Some(error) = self.error() {
            return Err(error);
        }
        let captured = self.check_capture(host_seconds())?;
        let mut ready = captured;
        for entry in &self.entries {
            ready &= entry
                .encoder
                .as_ref()
                .is_some_and(|e| matches!(e.state(), EncoderState::Ready));
            match self.previews.recording_frame(entry.config.id) {
                Some(Ok(frame)) => ready &= frame.canvas == entry.config.output.resolution,
                Some(Err(error)) => return Err(error),
                None => ready = false,
            }
        }
        if !ready && self.preparing_since.elapsed() > Duration::from_secs(10) {
            return Err(
                "Recording preparation timed out waiting for capture/render/writers".into(),
            );
        }
        Ok(ready)
    }

    fn set_timeline(&mut self, timeline: Timeline) {
        if self.timeline.is_none() {
            for entry in &mut self.entries {
                entry.last_audio = Instant::now();
                entry.last_frame = Instant::now();
            }
        }
        self.timeline = Some(timeline);
    }

    fn begin_finish(&mut self, end: f64) {
        if self.finish.is_none() && !self.closed {
            self.finish = Some((end, Instant::now()));
        }
    }

    fn begin_rollback(&mut self) {
        if let Some(routing) = &self.routing {
            routing.accepting.store(false, Ordering::SeqCst);
        }
        self.closed = true;
        self.pending.clear();
        for entry in &mut self.entries {
            if let Some(encoder) = &mut entry.encoder {
                encoder.cancel();
            }
        }
    }

    fn finalized(&mut self) -> bool {
        if !self.closed
            || self
                .routing
                .as_ref()
                .is_some_and(|routing| routing.publishing.load(Ordering::SeqCst) != 0)
        {
            return false;
        }
        let done = self.entries.iter().all(|entry| {
            entry.encoder.as_ref().is_none_or(|encoder| {
                matches!(
                    encoder.state(),
                    EncoderState::Saved(_) | EncoderState::Failed { .. } | EncoderState::Cancelled
                )
            })
        });
        if done {
            if let Some(error) = self.error() {
                self.fail(error);
            }
            let reports = self.reports();
            for (entry, mut report) in self.entries.iter_mut().zip(reports) {
                // Retain only plain Rust diagnostics after terminal completion,
                // not encoder queues containing pooled frame/sample references.
                entry.encoder.take();
                report.video_queue = 0;
                report.audio_queue = 0;
                entry.report = Some(report);
            }
            self.pending.clear();
            if let Some(routing) = &self.routing {
                while routing.pop_audio().is_some() {}
            }
            self.shared.routing.store(None);
            self.previews.record(Vec::new());
            self.routing = None;
        }
        done
    }

    fn reports(&self) -> Vec<RecordingReport> {
        self.entries
            .iter()
            .map(|entry| {
                if let Some(report) = &entry.report {
                    return report.clone();
                }
                let state = entry.encoder.as_ref().map(Encoder::state);
                let stats = entry
                    .encoder
                    .as_ref()
                    .map(Encoder::stats)
                    .unwrap_or_default();
                let (status, saved_path, error) = match state {
                    Some(EncoderState::Saved(path)) => ("Saved", Some(path), None),
                    Some(EncoderState::Failed { error, incomplete }) => (
                        "Failed",
                        None,
                        Some(format!("{error}; incomplete: {}", incomplete.display())),
                    ),
                    Some(EncoderState::Cancelled) => ("Cancelled", None, None),
                    Some(EncoderState::Finalizing) => ("Finalizing", None, None),
                    Some(EncoderState::Ready) => (
                        if self.finish.is_some() {
                            "Finalizing"
                        } else if self
                            .timeline
                            .as_ref()
                            .is_some_and(|t| t.map(host_seconds()).is_none())
                        {
                            "Paused"
                        } else if self.timeline.is_some() {
                            "Recording"
                        } else {
                            "Ready"
                        },
                        None,
                        None,
                    ),
                    _ => ("Preparing", None, None),
                };
                RecordingReport {
                    id: entry.config.id,
                    name: entry.config.name.clone(),
                    status: status.into(),
                    saved_path,
                    error: error.or_else(|| self.failure.clone()),
                    audio_level: entry.audio_level,
                    dropped_video: stats.dropped_video
                        + entry.cadence.dropped()
                        + entry.missing_video,
                    video_queue: stats.video_depth,
                    audio_queue: stats.audio_depth
                        + self
                            .routing
                            .as_ref()
                            .and_then(|r| r.depths.get(&entry.config.id))
                            .map_or(0, |depth| depth.load(Ordering::Relaxed))
                        + self
                            .pending
                            .iter()
                            .filter(|p| p.recording == entry.config.id)
                            .count(),
                }
            })
            .collect()
    }

    fn error(&self) -> Option<String> {
        self.failure
            .clone()
            .or_else(|| self.routing.as_ref().and_then(|r| r.error.get().cloned()))
            .or_else(|| {
                self.entries.iter().find_map(|e| {
                    e.encoder.as_ref().and_then(|e| {
                        e.error().or_else(|| {
                            if let EncoderState::Failed { error, .. } = e.state() {
                                Some(error)
                            } else {
                                None
                            }
                        })
                    })
                })
            })
    }
}

fn preflight(
    configs: &[RecordingConfig],
    view: &CaptureView,
) -> Result<Vec<CaptureTarget>, String> {
    if configs.is_empty() {
        return Err("Add a recording before starting".into());
    }
    let mut ids = HashSet::new();
    let mut targets = Vec::new();
    for config in configs {
        if !ids.insert(config.id) {
            return Err("Duplicate recording identity".into());
        }
        config.output.validate_format()?;
        crate::render::transform::validate(&config.viewport)?;
        if config.video_availability != SourceAvailability::Available
            || config.audio_availability != SourceAvailability::Available
        {
            return Err(format!(
                "{}: both sources must be available and authorized",
                config.name
            ));
        }
        let video = SourceKey::video(&config.video).ok_or("Select a video source")?;
        let audio = SourceKey::audio(&config.audio).ok_or("Select an audio source")?;
        for source in [video, audio] {
            let target = view
                .targets
                .iter()
                .find(|t| t.recording == config.id && t.request.source == source)
                .ok_or_else(|| {
                    format!("{}: selected source is not currently captured", config.name)
                })?;
            if !source.is_audio()
                && !target.request.format.supports(CaptureFormat::Video {
                    width: config.output.resolution.width,
                    height: config.output.resolution.height,
                    fps: config.output.fps,
                })
            {
                return Err(format!(
                    "{}: capture format is not ready for this output",
                    config.name
                ));
            }
            targets.push(target.clone());
        }
    }
    validate_live(&targets, view)?;
    Ok(targets)
}

fn validate_live(targets: &[CaptureTarget], view: &CaptureView) -> Result<(), String> {
    if let Some(error) = view.status.shutdown_errors.first() {
        return Err(error.clone());
    }
    let formats = negotiate(&view.targets).map_err(|e| e.to_string())?;
    for target in targets {
        if !view.targets.contains(target)
            || formats
                .get(&target.request.source)
                .is_none_or(|actual| !actual.format.supports(target.request.format))
        {
            return Err(
                "Recording capture demand changed; stop before changing sources/output".into(),
            );
        }
        match view
            .status
            .statuses
            .iter()
            .find(|s| s.recording == target.recording && s.source == target.request.source)
            .map(|s| &s.state)
        {
            Some(CaptureState::Receiving { .. }) => {}
            Some(CaptureState::Failed(error)) => return Err(error.to_string()),
            _ => return Err("Required video/audio source is not receiving samples".into()),
        }
    }
    Ok(())
}

struct AudioInfo {
    frames: usize,
    rate: u32,
    level: f32,
}

fn audio_info(sample: &NativeSample) -> Result<AudioInfo, String> {
    use objc2_core_audio_types::*;
    use objc2_core_media::CMAudioFormatDescriptionGetStreamBasicDescription;
    if sample.kind() != SampleKind::Audio {
        return Err("Expected PCM audio".into());
    }
    // SAFETY: Synchronous read of immutable sample/format. Only copied Rust
    // metadata and a bounded PCM meter copy escape; no native alias is retained.
    unsafe {
        let buffer = sample.buffer_read();
        let format = buffer.format_description().ok_or("Missing PCM format")?;
        let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(&format)
            .as_ref()
            .ok_or("Missing PCM description")?;
        if asbd.mFormatID != kAudioFormatLinearPCM
            || asbd.mSampleRate != 48_000.0
            || !(1..=2).contains(&asbd.mChannelsPerFrame)
            || !buffer.is_valid()
            || !buffer.data_is_ready()
            || buffer.num_samples() <= 0
            || buffer.num_samples() > 48_000
            || !sample.timestamp_seconds().is_finite()
            || sample.timestamp_seconds() < 0.0
        {
            return Err("Unsupported recording PCM format or timestamp".into());
        }
        let frames = buffer.num_samples() as usize;
        let mut level = 0.0_f32;
        let float = asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0;
        let signed = asbd.mFormatFlags & kAudioFormatFlagIsSignedInteger != 0;
        let bytes = (asbd.mBitsPerChannel / 8) as usize;
        let planar = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
        let channels = if planar {
            1
        } else {
            asbd.mChannelsPerFrame as usize
        };
        if asbd.mFormatFlags & kAudioFormatFlagIsBigEndian == 0
            && ((float && bytes == 4) || (signed && bytes == 2))
            && asbd.mBytesPerFrame as usize == channels * bytes
        {
            #[repr(C)]
            struct Buffers {
                count: u32,
                buffers: [AudioBuffer; 2],
            }
            let count = frames.min(1024);
            let length = count * channels * bytes;
            let mut data = [vec![0u8; length], vec![0u8; length]];
            let mut list = Buffers {
                count: if planar { asbd.mChannelsPerFrame } else { 1 },
                buffers: std::array::from_fn(|i| AudioBuffer {
                    mNumberChannels: channels as u32,
                    mDataByteSize: length as u32,
                    mData: data[i].as_mut_ptr().cast(),
                }),
            };
            if buffer.copy_pcm_data_into_audio_buffer_list(
                0,
                count as i32,
                std::ptr::NonNull::from(&mut list).cast(),
            ) != 0
            {
                return Err("Cannot read PCM meter samples".into());
            }
            for plane in data.iter().take(list.count as usize) {
                for value in plane.chunks_exact(bytes) {
                    let value = if float {
                        f32::from_ne_bytes(value.try_into().unwrap())
                    } else {
                        f32::from(i16::from_ne_bytes(value.try_into().unwrap())) / 32768.0
                    };
                    if value.is_finite() {
                        level = level.max(value.abs().min(1.0));
                    }
                }
            }
        }
        Ok(AudioInfo {
            frames,
            rate: asbd.mSampleRate as u32,
            level,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{CaptureRequest, service::CaptureStatus};
    use crate::model::{AudioSourceSelector, DisplayId, Resolution, VideoSourceSelector};

    fn tone() -> NativeSample {
        use objc2_core_audio_types::*;
        use objc2_core_foundation::CFRetained;
        use objc2_core_media::*;
        use std::ptr::{NonNull, null, null_mut};
        // SAFETY: Fixed-size interleaved float PCM. Core Media copies the local
        // Vec into its own block buffer before NativeSample can cross threads.
        unsafe {
            let mut description = AudioStreamBasicDescription {
                mSampleRate: 48_000.0,
                mFormatID: kAudioFormatLinearPCM,
                mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
                mBytesPerPacket: 8,
                mFramesPerPacket: 1,
                mBytesPerFrame: 8,
                mChannelsPerFrame: 2,
                mBitsPerChannel: 32,
                mReserved: 0,
            };
            let mut raw_format = null();
            assert_eq!(
                CMAudioFormatDescriptionCreate(
                    None,
                    NonNull::from(&mut description),
                    0,
                    null(),
                    0,
                    null(),
                    None,
                    NonNull::from(&mut raw_format)
                ),
                0
            );
            let format = CFRetained::from_raw(NonNull::new(raw_format.cast_mut()).unwrap());
            let timing = CMSampleTimingInfo {
                duration: CMTime {
                    value: 1,
                    timescale: 48_000,
                    flags: CMTimeFlags::Valid,
                    epoch: 0,
                },
                presentationTimeStamp: CMTime {
                    value: 0,
                    timescale: 48_000,
                    flags: CMTimeFlags::Valid,
                    epoch: 0,
                },
                decodeTimeStamp: kCMTimeInvalid,
            };
            let mut raw = null_mut();
            assert_eq!(
                CMSampleBuffer::create_ready(
                    None,
                    None,
                    Some(&format),
                    480,
                    1,
                    &timing,
                    1,
                    &8,
                    NonNull::from(&mut raw)
                ),
                0
            );
            let sample = CFRetained::from_raw(NonNull::new(raw).unwrap());
            let mut data = (0..480)
                .flat_map(|i| {
                    let value = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.2;
                    [value, -value]
                })
                .collect::<Vec<_>>();
            let mut list = AudioBufferList {
                mNumberBuffers: 1,
                mBuffers: [AudioBuffer {
                    mNumberChannels: 2,
                    mDataByteSize: (data.len() * 4) as u32,
                    mData: data.as_mut_ptr().cast(),
                }],
            };
            assert_eq!(
                sample.set_data_buffer_from_audio_buffer_list(
                    None,
                    None,
                    0,
                    NonNull::from(&mut list)
                ),
                0
            );
            NativeSample::retain(&sample, SampleKind::Audio).unwrap()
        }
    }

    fn configs(directory: &std::path::Path) -> Vec<RecordingConfig> {
        (0..2)
            .map(|index| {
                let mut config = RecordingConfig::new(format!("runtime-{index}"));
                config.video = VideoSourceSelector::Display {
                    display_id: DisplayId(1),
                    label: "Synthetic display".into(),
                };
                config.audio = AudioSourceSelector::System;
                config.video_availability = SourceAvailability::Available;
                config.audio_availability = SourceAvailability::Available;
                config.output.resolution = Resolution {
                    width: 80,
                    height: 80,
                };
                config.output.fps = 10;
                config.output.directory = directory.to_owned();
                config.viewport.crop_width = 0.5;
                config
            })
            .collect()
    }

    fn capture(configs: &[RecordingConfig]) -> CaptureView {
        let targets = configs
            .iter()
            .flat_map(|config| {
                [
                    CaptureTarget {
                        recording: config.id,
                        request: CaptureRequest {
                            source: SourceKey::video(&config.video).unwrap(),
                            format: CaptureFormat::Video {
                                width: config.output.resolution.width,
                                height: config.output.resolution.height,
                                fps: config.output.fps,
                            },
                        },
                    },
                    CaptureTarget {
                        recording: config.id,
                        request: CaptureRequest {
                            source: SourceKey::audio(&config.audio).unwrap(),
                            format: CaptureFormat::Audio {
                                sample_rate: 48_000,
                                channels: 2,
                            },
                        },
                    },
                ]
            })
            .collect::<Vec<_>>();
        CaptureView {
            status: CaptureSnapshot {
                generation: 1,
                active_sessions: 2,
                shutdown_errors: Vec::new(),
                statuses: targets
                    .iter()
                    .map(|target| CaptureStatus {
                        recording: target.recording,
                        source: target.request.source.clone(),
                        state: CaptureState::Receiving {
                            samples: 1,
                            dropped: 0,
                        },
                    })
                    .collect(),
            },
            targets,
            epoch: None,
            host: host_seconds(),
        }
    }

    fn shared(view: CaptureView) -> Arc<Shared> {
        Arc::new(Shared {
            capture: ArcSwap::from_pointee(view),
            routing: ArcSwapOption::empty(),
            snapshot: ArcSwap::from_pointee(GroupSnapshot::default()),
            finished: AtomicBool::new(false),
        })
    }

    struct NativeFixture {
        directory: tempfile::TempDir,
        configs: Vec<RecordingConfig>,
        view: CaptureView,
        previews: PreviewBridge,
        runtime: RecordingRuntime,
        stop: Arc<AtomicBool>,
        render_stop: Arc<AtomicBool>,
        tone: NativeSample,
        audio_start: f64,
    }

    impl NativeFixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let configs = configs(directory.path());
            let view = capture(&configs);
            let stop = Arc::new(AtomicBool::new(false));
            let render_stop = Arc::new(AtomicBool::new(false));
            let previews = PreviewBridge::new(render_stop.clone());
            for config in &configs {
                previews.synthetic_input(
                    config.id,
                    SourceKey::Display(1),
                    crate::render::synthetic_sample(80, 40)
                        .unwrap()
                        .with_host_timestamp(host_seconds())
                        .unwrap(),
                );
            }
            previews.configure(
                configs
                    .iter()
                    .map(|config| PreviewConfig {
                        recording: config.id,
                        source: SourceKey::Display(1),
                        transform: config.viewport,
                        canvas: config.output.resolution,
                        fps: config.output.fps,
                        active: false,
                        visible: false,
                    })
                    .collect(),
            );
            let runtime = RecordingRuntime::new(previews.clone(), stop.clone());
            runtime.capture_status(view.status.clone(), view.targets.clone(), None);
            Self {
                directory,
                configs,
                view,
                previews,
                runtime,
                stop,
                render_stop,
                tone: tone(),
                audio_start: host_seconds(),
            }
        }

        fn pump(&mut self) {
            let now = host_seconds();
            let routing = self.runtime.routing();
            self.runtime.capture_status(
                self.view.status.clone(),
                self.view.targets.clone(),
                routing.as_deref(),
            );
            if let Some(routing) = routing {
                while self.audio_start + 0.01 <= now {
                    let sample = self
                        .tone
                        .clone()
                        .with_host_timestamp(self.audio_start)
                        .unwrap();
                    for target in self
                        .view
                        .targets
                        .iter()
                        .filter(|t| t.request.source.is_audio())
                    {
                        routing.audio(target, sample.clone());
                    }
                    self.audio_start += 0.01;
                }
            } else {
                self.audio_start = now;
            }
        }

        fn wait(&mut self, sequence: u64, state: GroupState) -> GroupSnapshot {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                self.pump();
                let snapshot = self.runtime.snapshot();
                if snapshot.command_sequence >= sequence && snapshot.state == state {
                    return snapshot;
                }
                assert!(
                    state == GroupState::Failed || snapshot.state != GroupState::Failed,
                    "{snapshot:?}"
                );
                assert!(
                    Instant::now() < deadline,
                    "Timed out waiting for {state:?}: {snapshot:?}"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        fn run_for(&mut self, duration: Duration) {
            let deadline = Instant::now() + duration;
            while Instant::now() < deadline {
                self.pump();
                assert!(
                    self.runtime.snapshot().error.is_none(),
                    "{:?}",
                    self.runtime.snapshot()
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    impl Drop for NativeFixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            let deadline = Instant::now() + Duration::from_secs(35);
            while !self.runtime.is_finished() && Instant::now() < deadline {
                self.pump();
                std::thread::sleep(Duration::from_millis(4));
            }
            self.render_stop.store(true, Ordering::Release);
        }
    }

    #[allow(deprecated)]
    fn inspect_mp4(path: &std::path::Path, expected_duration: f64, changed: bool) {
        use objc2_av_foundation::{
            AVAssetReader, AVAssetReaderStatus, AVAssetReaderTrackOutput, AVMediaTypeAudio,
            AVMediaTypeVideo, AVURLAsset,
        };
        use objc2_core_video::*;
        use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};
        // SAFETY: Local read-only assets/readers. No aliases escape and sample
        // timestamps are copied to Rust scalars before each buffer is released.
        objc2::rc::autoreleasepool(|_| unsafe {
            let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str().unwrap()));
            let asset = AVURLAsset::URLAssetWithURL_options(&url, None);
            assert!((asset.duration().seconds() - expected_duration).abs() < 0.1);
            for media in [AVMediaTypeVideo.unwrap(), AVMediaTypeAudio.unwrap()] {
                let tracks = asset.tracksWithMediaType(media);
                assert_eq!(tracks.count(), 1);
                let track = tracks.objectAtIndex(0);
                if media == AVMediaTypeVideo.unwrap() {
                    assert_eq!(
                        (track.naturalSize().width, track.naturalSize().height),
                        (80.0, 80.0)
                    );
                }
                let reader = AVAssetReader::assetReaderWithAsset_error(&asset).unwrap();
                let key: &NSString =
                    &*(kCVPixelBufferPixelFormatTypeKey as *const _ as *const NSString);
                let value = NSNumber::new_u32(kCVPixelFormatType_32BGRA);
                let settings = NSDictionary::from_slices(&[key], &[&*value]);
                let output =
                    AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(
                        &track,
                        (media == AVMediaTypeVideo.unwrap()).then(|| settings.cast_unchecked()),
                    );
                reader.addOutput(&output);
                assert!(reader.startReading());
                let mut timestamps = Vec::new();
                let mut sample_count = 0;
                let mut landmarks = Vec::new();
                while let Some(sample) = output.copyNextSampleBuffer() {
                    if sample.num_samples() > 0 {
                        sample_count += sample.num_samples();
                        assert!(sample.total_sample_size() > 0);
                        if media == AVMediaTypeAudio.unwrap() {
                            assert_eq!(
                                sample.format_description().unwrap().media_sub_type(),
                                objc2_core_audio_types::kAudioFormatMPEG4AAC
                            );
                        } else {
                            let image = sample.image_buffer().unwrap();
                            assert_eq!(
                                CVPixelBufferGetPixelFormatType(&image),
                                kCVPixelFormatType_32BGRA
                            );
                            assert_eq!(
                                (
                                    CVPixelBufferGetWidth(&image),
                                    CVPixelBufferGetHeight(&image)
                                ),
                                (80, 80)
                            );
                            assert_eq!(
                                CVPixelBufferLockBaseAddress(
                                    &image,
                                    CVPixelBufferLockFlags::ReadOnly
                                ),
                                0
                            );
                            let pixel = CVPixelBufferGetBaseAddress(&image)
                                .cast::<u8>()
                                .add(20 * CVPixelBufferGetBytesPerRow(&image) + 20 * 4);
                            landmarks.push(std::ptr::read_unaligned(pixel.cast::<[u8; 4]>()));
                            assert_eq!(
                                CVPixelBufferUnlockBaseAddress(
                                    &image,
                                    CVPixelBufferLockFlags::ReadOnly
                                ),
                                0
                            );
                        }
                        timestamps.push(sample.presentation_time_stamp().seconds());
                    }
                }
                assert_eq!(reader.status(), AVAssetReaderStatus::Completed);
                // A compressed audio buffer can contain many AAC packets. Count
                // native samples, not delivery buffers, for actual media coverage.
                assert!(
                    sample_count >= 3,
                    "Expected native encoded samples: {sample_count}, {timestamps:?}"
                );
                assert!(timestamps.windows(2).all(|pair| pair[0] < pair[1]));
                assert!(timestamps.last().unwrap() < &(expected_duration + 0.03));
                if media == AVMediaTypeVideo.unwrap() {
                    let red = |pixel: &[u8; 4]| pixel[2] > 150 && pixel[1] < 100;
                    let green = |pixel: &[u8; 4]| pixel[1] > 150 && pixel[2] < 100;
                    assert!(
                        red(landmarks.first().unwrap()),
                        "Initial left crop: {landmarks:?}"
                    );
                    if changed {
                        assert!(
                            green(landmarks.last().unwrap()),
                            "Live right crop must reach the saved MP4: {landmarks:?}"
                        );
                    } else {
                        assert!(
                            landmarks.iter().all(red),
                            "Other recording must remain independent: {landmarks:?}"
                        );
                    }
                }
            }
        });
    }

    #[test]
    fn native_runtime_routes_hidden_outputs_pause_live_edit_and_asynchronous_shutdown() {
        let mut fixture = NativeFixture::new();
        let start = fixture
            .runtime
            .command(GroupCommand::Start, fixture.configs.clone())
            .unwrap();
        fixture.wait(start, GroupState::Recording);
        fixture.run_for(Duration::from_millis(250));
        assert!(
            fixture
                .runtime
                .snapshot()
                .recordings
                .iter()
                .all(|r| r.audio_level > 0.1)
        );
        let pause = fixture
            .runtime
            .command(GroupCommand::Pause, Vec::new())
            .unwrap();
        let paused = fixture.wait(pause, GroupState::Paused).elapsed;
        fixture.run_for(Duration::from_millis(100));
        assert_eq!(fixture.runtime.snapshot().elapsed, paused);
        let mut configs = fixture.previews.configs.load_full().as_ref().clone();
        configs[1].transform.crop_x = 0.5;
        fixture.previews.configure(configs.clone());
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            fixture.pump();
            if let Some(Ok(frame)) = fixture.previews.recording_frame(configs[1].recording)
                && frame.transform == configs[1].transform
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Hidden paused transform was not rendered"
            );
            std::thread::sleep(Duration::from_millis(4));
        }
        assert_eq!(
            fixture
                .previews
                .recording_frame(configs[0].recording)
                .unwrap()
                .unwrap()
                .transform,
            configs[0].transform
        );
        let resume = fixture
            .runtime
            .command(GroupCommand::Resume, Vec::new())
            .unwrap();
        fixture.wait(resume, GroupState::Recording);
        fixture.run_for(Duration::from_millis(250));
        fixture.stop.store(true, Ordering::Release);
        let finishing = fixture.wait(resume, GroupState::Finalizing);
        assert!(!fixture.previews.is_finished());
        assert!(!fixture.runtime.is_finished());
        let saved = fixture.wait(resume, GroupState::Idle);
        assert!((saved.elapsed - finishing.elapsed).abs() < 1e-9);
        assert!(saved.elapsed >= 0.5 && saved.elapsed < 1.0, "{saved:?}");
        assert_eq!(saved.recordings.len(), 2);
        for (index, report) in saved.recordings.iter().enumerate() {
            assert!(report.error.is_none(), "{report:?}");
            inspect_mp4(
                report.saved_path.as_ref().unwrap(),
                saved.elapsed,
                index == 1,
            );
        }
        assert_eq!(
            std::fs::read_dir(fixture.directory.path()).unwrap().count(),
            2
        );
    }

    #[test]
    fn native_runtime_source_loss_stops_the_entire_group_without_reopening_capture() {
        let mut fixture = NativeFixture::new();
        let start = fixture
            .runtime
            .command(GroupCommand::Start, fixture.configs.clone())
            .unwrap();
        fixture.wait(start, GroupState::Recording);
        fixture.run_for(Duration::from_millis(220));
        fixture.view.status.statuses[0].state = CaptureState::Failed(
            crate::capture::CaptureError::Unavailable("synthetic source lost".into()),
        );
        let failed = fixture.wait(start, GroupState::Failed);
        assert!(
            failed
                .error
                .as_ref()
                .unwrap()
                .contains("synthetic source lost")
        );
        assert!(
            failed
                .recordings
                .iter()
                .all(|report| report.error.is_some())
        );
        assert!(fixture.runtime.routing().is_none());
        assert!(
            failed.elapsed < 0.5,
            "Stale video was recorded after source failure: {failed:?}"
        );
    }

    #[test]
    fn bounded_audio_handoff_latches_overflow_and_pcm_meter_reads_owned_samples() {
        let fixture = NativeFixture::new();
        let routing = Routing {
            epoch: 1,
            targets: fixture.view.targets.clone(),
            depths: fixture
                .configs
                .iter()
                .map(|config| (config.id, AtomicUsize::new(0)))
                .collect(),
            audio: ArrayQueue::new(2),
            error: OnceLock::new(),
            accepting: AtomicBool::new(true),
            publishing: AtomicUsize::new(0),
        };
        let target = fixture
            .view
            .targets
            .iter()
            .find(|t| t.request.source.is_audio())
            .unwrap();
        routing.audio(target, fixture.tone.clone());
        routing.audio(target, fixture.tone.clone());
        assert!(routing.error.get().is_none());
        routing.audio(target, fixture.tone.clone());
        assert!(routing.error.get().unwrap().contains("overflow"));
        assert_eq!(routing.audio.len(), 2);
        assert_eq!(routing.publishing.load(Ordering::SeqCst), 0);
        let info = audio_info(&fixture.tone).unwrap();
        assert_eq!((info.frames, info.rate), (480, 48_000));
        assert!(info.level > 0.19 && info.level <= 0.2);
        assert!(audio_info(&crate::render::synthetic_sample(80, 40).unwrap()).is_err());
        routing.accepting.store(false, Ordering::SeqCst);
        routing.pop_audio();
        routing.audio(target, fixture.tone.clone());
        assert_eq!(
            routing.audio.len(),
            1,
            "Admission must stay closed beyond the drain boundary"
        );
    }

    #[test]
    fn preflight_requires_exact_live_sources_formats_and_valid_transforms() {
        let directory = tempfile::tempdir().unwrap();
        let configs = configs(directory.path());
        let mut view = capture(&configs);
        assert_eq!(preflight(&configs, &view).unwrap().len(), 4);
        let mut invalid = configs.clone();
        invalid[0].output.fps = 11;
        assert!(
            preflight(&invalid, &view)
                .unwrap_err()
                .contains("capture format")
        );
        invalid = configs.clone();
        invalid[0].viewport.scale = f64::NAN;
        assert!(preflight(&invalid, &view).is_err());
        invalid = configs.clone();
        invalid[0].audio = AudioSourceSelector::None;
        assert!(preflight(&invalid, &view).is_err());
        invalid = configs.clone();
        invalid[1].id = invalid[0].id;
        assert!(preflight(&invalid, &view).is_err());
        view.status.statuses[0].state = CaptureState::Waiting;
        assert!(
            preflight(&configs, &view)
                .unwrap_err()
                .contains("not receiving")
        );
        view.status.statuses[0].state = CaptureState::Failed(
            crate::capture::CaptureError::Unavailable("lost display".into()),
        );
        assert!(
            preflight(&configs, &view)
                .unwrap_err()
                .contains("lost display")
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn all_destinations_are_reserved_before_any_encoder_and_partial_reservations_roll_back() {
        let directory = tempfile::tempdir().unwrap();
        let mut configs = configs(directory.path());
        let blocked = directory.path().join("not-a-directory");
        std::fs::write(&blocked, b"user file").unwrap();
        configs[1].output.directory = blocked.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let previews = PreviewBridge::new(stop.clone());
        let shared = shared(capture(&configs));
        let mut backend = NativeGroupBackend::new(shared.clone(), previews);
        assert!(backend.prepare(&configs).is_err());
        assert!(backend.entries.iter().all(|entry| entry.encoder.is_none()));
        backend.begin_rollback();
        assert!(backend.finalized());
        assert!(shared.routing.load().is_none());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        assert_eq!(std::fs::read(&blocked).unwrap(), b"user file");
        stop.store(true, Ordering::Release);
    }

    #[test]
    fn command_sequences_are_bounded_ordered_and_not_acknowledged_on_enqueue() {
        let (sender, receiver) = mpsc::sync_channel(16);
        let runtime = RecordingRuntime {
            shared: shared(CaptureView::default()),
            commands: Arc::new(Mutex::new(Commands { sender, next: 1 })),
            shutdown: Arc::new(AtomicBool::new(false)),
        };
        for expected in 1..=16 {
            assert_eq!(
                runtime.command(GroupCommand::Start, Vec::new()).unwrap(),
                expected
            );
            assert_eq!(runtime.snapshot().command_sequence, 0);
        }
        assert!(
            runtime
                .command(GroupCommand::Stop, Vec::new())
                .unwrap_err()
                .contains("full")
        );
        for expected in 1..=16 {
            assert_eq!(receiver.try_recv().unwrap().sequence, expected);
        }
        assert_eq!(runtime.command(GroupCommand::Stop, Vec::new()).unwrap(), 17);
        let held = runtime.commands.lock().unwrap();
        let start = Instant::now();
        assert!(
            runtime
                .command(GroupCommand::Stop, Vec::new())
                .unwrap_err()
                .contains("busy")
        );
        assert!(start.elapsed() < Duration::from_millis(50));
        drop(held);
        runtime.shutdown.store(true, Ordering::Release);
        assert!(runtime.command(GroupCommand::Start, Vec::new()).is_err());
    }

    #[test]
    fn rejected_start_acknowledges_only_its_own_sequence_and_runtime_shuts_down() {
        let stop = Arc::new(AtomicBool::new(false));
        let render_stop = Arc::new(AtomicBool::new(false));
        let previews = PreviewBridge::new(render_stop.clone());
        let runtime = RecordingRuntime::new(previews.clone(), stop.clone());
        let wait = |sequence| {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let snapshot = runtime.snapshot();
                if snapshot.command_sequence == sequence {
                    return snapshot;
                }
                assert!(
                    Instant::now() < deadline,
                    "Missing command acknowledgment: {snapshot:?}"
                );
                std::thread::sleep(Duration::from_millis(4));
            }
        };
        let first = runtime.command(GroupCommand::Start, Vec::new()).unwrap();
        let old_failed = wait(first);
        assert_eq!(old_failed.state, GroupState::Failed);
        let second = runtime.command(GroupCommand::Start, Vec::new()).unwrap();
        assert!(old_failed.command_sequence < second);
        assert_eq!(wait(second).state, GroupState::Failed);
        stop.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !runtime.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(4));
        }
        assert!(runtime.is_finished());
        assert!(!previews.is_finished());
        render_stop.store(true, Ordering::Release);
    }
}
