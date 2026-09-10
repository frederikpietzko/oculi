//! Serialized native capture owner. AppKit exchanges only Rust demand/status values.

use std::collections::HashMap;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use super::{device_capture, sample::NativeSample, screen_capture};
use crate::capture::registry::SamplePublisher;
use crate::capture::service::{CaptureService, CaptureSnapshot, CaptureTarget};
use crate::capture::{CaptureBackend, CaptureError, CaptureRequest, CaptureSession, SourceKey};

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
        let (input, output, stopping, done) = (
            demand.clone(),
            snapshot.clone(),
            stop.clone(),
            finished.clone(),
        );
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(|| {
                let mut service = CaptureService::new(NativeBackend);
                while !stopping.load(Ordering::Acquire) {
                    // Release all command locks before entering Apple frameworks.
                    let next = input.lock().unwrap().take();
                    objc2::rc::autoreleasepool(|_| {
                        if let Some(Demand {
                            generation,
                            targets,
                            retry,
                        }) = next
                        {
                            service.reconcile(generation, targets, retry);
                        }
                        *output.lock().unwrap() = Some(service.poll());
                    });
                    std::thread::sleep(Duration::from_millis(20));
                }
                *output.lock().unwrap() = Some(objc2::rc::autoreleasepool(|_| service.shutdown()));
            });
            if result.is_err() {
                *output.lock().unwrap_or_else(|p| p.into_inner()) = Some(CaptureSnapshot {
                    shutdown_errors: vec![
                        "Capture worker stopped unexpectedly. Restart Oculi.".into(),
                    ],
                    ..Default::default()
                });
            }
            done.store(true, Ordering::Release);
        });
        Self {
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

    pub fn poll(&self) -> Option<CaptureSnapshot> {
        let result = self.snapshot.try_lock().ok()?.take()?;
        (result.generation == self.generation || !result.shutdown_errors.is_empty())
            .then_some(result)
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Release);
    }
    pub fn is_stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

impl Drop for CaptureWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mailbox() -> CaptureWorker {
        CaptureWorker {
            demand: Arc::new(Mutex::new(None)),
            snapshot: Arc::new(Mutex::new(None)),
            stop: Arc::new(AtomicBool::new(false)),
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
