//! Per-recording demand and immutable capture diagnostics, independent of AppKit.

use std::collections::HashMap;

use super::{
    CaptureBackend,
    registry::{CaptureLease, CaptureRegistry, SubscriptionPolicy},
};
use super::{CaptureError, CaptureRequest, SourceKey};
use crate::model::RecordingId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureTarget {
    pub recording: RecordingId,
    pub request: CaptureRequest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureState {
    Waiting,
    Receiving { samples: u64, dropped: u64 },
    Failed(CaptureError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureStatus {
    pub recording: RecordingId,
    pub source: SourceKey,
    pub state: CaptureState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaptureSnapshot {
    pub generation: u64,
    pub statuses: Vec<CaptureStatus>,
    pub active_sessions: usize,
    pub shutdown_errors: Vec<String>,
}

/// Resolve all demands before touching devices, so a small first subscriber cannot
/// accidentally select the shared format for a larger second output.
pub fn negotiate(
    targets: &[CaptureTarget],
) -> Result<HashMap<SourceKey, CaptureRequest>, CaptureError> {
    let mut requests: HashMap<SourceKey, CaptureRequest> = HashMap::new();
    for target in targets {
        let request = &target.request;
        request.format.validate(&request.source)?;
        if let Some(existing) = requests.get_mut(&request.source) {
            existing.format = existing.format.merge(request.format)?;
        } else {
            requests.insert(request.source.clone(), request.clone());
        }
    }
    Ok(requests)
}

struct ActiveTarget<T: Clone + Send + 'static> {
    target: CaptureTarget,
    lease: CaptureLease<T>,
    samples: u64,
}

/// Diagnostic video is latest-only; audio is bounded and drained every poll.
/// Recording writers will acquire their own subscriptions rather than reuse diagnostics.
pub struct CaptureService<T: Clone + Send + 'static, B: CaptureBackend<T>> {
    registry: CaptureRegistry<T, B>,
    active: Vec<ActiveTarget<T>>,
    formats: HashMap<SourceKey, CaptureRequest>,
    failures: HashMap<SourceKey, CaptureError>,
    targets: Vec<CaptureTarget>,
    snapshot: CaptureSnapshot,
}

impl<T: Clone + Send + 'static, B: CaptureBackend<T>> CaptureService<T, B> {
    pub fn new(backend: B) -> Self {
        Self {
            registry: CaptureRegistry::new(backend),
            active: Vec::new(),
            formats: HashMap::new(),
            failures: HashMap::new(),
            targets: Vec::new(),
            snapshot: CaptureSnapshot::default(),
        }
    }

    pub fn reconcile(&mut self, generation: u64, targets: Vec<CaptureTarget>, retry: bool) {
        self.snapshot.generation = generation;
        let requests = match negotiate(&targets) {
            Ok(requests) => requests,
            Err(error) => {
                self.active.clear();
                self.record_teardown();
                self.formats.clear();
                self.failures = targets
                    .iter()
                    .map(|t| (t.request.source.clone(), error.clone()))
                    .collect();
                self.targets = targets;
                return;
            }
        };
        // Reconfiguration releases every dependent lease before reopening an
        // exclusive device. Lower demands reuse the existing larger stream.
        self.active.retain(|active| {
            targets.contains(&active.target)
                && requests
                    .get(&active.target.request.source)
                    .is_some_and(|request| {
                        self.formats
                            .get(&request.source)
                            .is_some_and(|old| old.format.supports(request.format))
                    })
                && !(retry && active.lease.error().is_some())
        });
        self.record_teardown();
        self.formats.retain(|source, _| {
            self.active
                .iter()
                .any(|a| &a.target.request.source == source)
        });
        self.failures.retain(|source, _| {
            requests.contains_key(source)
                && !retry
                && targets
                    .iter()
                    .any(|t| &t.request.source == source && self.targets.contains(t))
        });
        for target in &targets {
            if self.active.iter().any(|active| &active.target == target)
                || self.failures.contains_key(&target.request.source)
            {
                continue;
            }
            let request = &requests[&target.request.source];
            let policy = if request.source.is_audio() {
                SubscriptionPolicy::Bounded {
                    capacity: 256,
                    audio: true,
                }
            } else {
                SubscriptionPolicy::Latest
            };
            match self.registry.subscribe(request.clone(), policy) {
                Ok(lease) => {
                    self.formats.insert(request.source.clone(), request.clone());
                    self.active.push(ActiveTarget {
                        target: target.clone(),
                        lease,
                        samples: 0,
                    });
                }
                Err(error) => {
                    self.failures.insert(request.source.clone(), error);
                }
            }
        }
        self.targets = targets;
    }

    fn record_teardown(&mut self) {
        if let Err(error) = self.registry.reap() {
            self.snapshot.shutdown_errors.push(error.to_string());
        }
    }

    pub fn poll(&mut self) -> CaptureSnapshot {
        for active in &mut self.active {
            for _ in 0..256 {
                if active.lease.try_recv().is_none() {
                    break;
                }
                active.samples += 1;
            }
            if let Some(error) = active.lease.error() {
                self.failures
                    .insert(active.target.request.source.clone(), error);
            }
        }
        // Stop a failed shared source, but retain its error until explicit retry
        // or selection change. Polling must not repeatedly reopen a busy device.
        self.active
            .retain(|active| !self.failures.contains_key(&active.target.request.source));
        self.record_teardown();
        self.snapshot.active_sessions = self.registry.session_count();
        self.snapshot.statuses = self
            .targets
            .iter()
            .map(|target| {
                let state = if let Some(error) = self.failures.get(&target.request.source) {
                    CaptureState::Failed(error.clone())
                } else if let Some(active) =
                    self.active.iter().find(|active| &active.target == target)
                {
                    if active.samples == 0 {
                        CaptureState::Waiting
                    } else {
                        CaptureState::Receiving {
                            samples: active.samples,
                            dropped: active.lease.stats().dropped,
                        }
                    }
                } else {
                    CaptureState::Waiting
                };
                CaptureStatus {
                    recording: target.recording,
                    source: target.request.source.clone(),
                    state,
                }
            })
            .collect();
        self.snapshot.clone()
    }

    pub fn shutdown(&mut self) -> CaptureSnapshot {
        self.active.clear();
        if let Err(error) = self.registry.shutdown() {
            self.snapshot.shutdown_errors.push(error.to_string());
        }
        self.snapshot.active_sessions = self.registry.session_count();
        self.snapshot.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::CaptureFormat;

    fn target(source: SourceKey, format: CaptureFormat) -> CaptureTarget {
        CaptureTarget {
            recording: RecordingId::new(),
            request: CaptureRequest { source, format },
        }
    }

    #[test]
    fn demand_is_negotiated_before_start_and_order_independent() {
        let small = target(
            SourceKey::Camera("camera".into()),
            CaptureFormat::Video {
                width: 1280,
                height: 720,
                fps: 60,
            },
        );
        let large = target(
            SourceKey::Camera("camera".into()),
            CaptureFormat::Video {
                width: 1920,
                height: 1080,
                fps: 30,
            },
        );
        let forward = negotiate(&[small.clone(), large.clone()]).unwrap();
        let reverse = negotiate(&[large, small]).unwrap();
        assert_eq!(forward, reverse);
        assert_eq!(forward.len(), 1);
        assert_eq!(
            forward.values().next().unwrap().format,
            CaptureFormat::Video {
                width: 1920,
                height: 1080,
                fps: 60
            }
        );
    }

    #[test]
    fn incompatible_audio_or_media_mismatch_fails_before_start() {
        let source = SourceKey::SystemAudio;
        let a = target(
            source.clone(),
            CaptureFormat::Audio {
                sample_rate: 48_000,
                channels: 1,
            },
        );
        let b = target(
            source.clone(),
            CaptureFormat::Audio {
                sample_rate: 48_000,
                channels: 2,
            },
        );
        assert!(negotiate(&[a, b]).is_err());
        assert!(
            negotiate(&[target(
                source,
                CaptureFormat::Video {
                    width: 1920,
                    height: 1080,
                    fps: 30
                }
            )])
            .is_err()
        );
    }

    #[derive(Default)]
    struct FakeState {
        starts: Vec<CaptureRequest>,
        stops: usize,
        publishers: Vec<crate::capture::registry::SamplePublisher<u64>>,
        fail_start: bool,
    }
    struct Backend(std::rc::Rc<std::cell::RefCell<FakeState>>);
    struct Session(std::rc::Rc<std::cell::RefCell<FakeState>>);
    impl crate::capture::CaptureSession for Session {
        fn stop(&mut self) -> Result<(), CaptureError> {
            self.0.borrow_mut().stops += 1;
            Ok(())
        }
    }
    impl CaptureBackend<u64> for Backend {
        fn start(
            &mut self,
            request: &CaptureRequest,
            publisher: crate::capture::registry::SamplePublisher<u64>,
        ) -> Result<Box<dyn crate::capture::CaptureSession>, CaptureError> {
            let mut state = self.0.borrow_mut();
            state.starts.push(request.clone());
            if state.fail_start {
                return Err(CaptureError::Native("device busy".into()));
            }
            state.publishers.push(publisher);
            Ok(Box::new(Session(self.0.clone())))
        }
    }
    fn video(width: u32) -> CaptureTarget {
        target(
            SourceKey::Camera("camera".into()),
            CaptureFormat::Video {
                width,
                height: 1080,
                fps: 30,
            },
        )
    }

    #[test]
    fn service_shares_source_and_removing_one_recording_keeps_other_running() {
        let state = std::rc::Rc::new(std::cell::RefCell::new(FakeState::default()));
        let mut service = CaptureService::new(Backend(state.clone()));
        let a = video(1280);
        let b = video(1920);
        service.reconcile(1, vec![a.clone(), b.clone()], false);
        assert_eq!(state.borrow().starts.len(), 1);
        assert_eq!(state.borrow().starts[0].format, b.request.format);
        state.borrow().publishers[0].publish(42);
        let snapshot = service.poll();
        assert_eq!(snapshot.statuses.len(), 2);
        assert!(
            snapshot
                .statuses
                .iter()
                .all(|s| matches!(s.state, CaptureState::Receiving { .. }))
        );
        service.reconcile(2, vec![a], false);
        assert_eq!(state.borrow().stops, 0);
        assert_eq!(service.poll().active_sessions, 1);
        assert_eq!(service.shutdown().active_sessions, 0);
        assert_eq!(state.borrow().stops, 1);
    }

    #[test]
    fn higher_demand_restarts_exclusive_source_after_all_old_leases_release() {
        let state = std::rc::Rc::new(std::cell::RefCell::new(FakeState::default()));
        let mut service = CaptureService::new(Backend(state.clone()));
        let a = video(1280);
        service.reconcile(1, vec![a.clone()], false);
        service.reconcile(2, vec![a, video(1920)], false);
        assert_eq!(state.borrow().starts.len(), 2);
        assert_eq!(state.borrow().stops, 1);
        assert_eq!(service.poll().active_sessions, 1);
        service.shutdown();
    }

    #[test]
    fn failed_source_fans_out_stops_and_waits_for_explicit_retry() {
        let state = std::rc::Rc::new(std::cell::RefCell::new(FakeState::default()));
        let mut service = CaptureService::new(Backend(state.clone()));
        let targets = vec![video(1920), video(1920)];
        service.reconcile(1, targets.clone(), false);
        state.borrow().publishers[0].fail(CaptureError::Unavailable("disconnected".into()));
        let snapshot = service.poll();
        assert_eq!(snapshot.active_sessions, 0);
        assert_eq!(state.borrow().stops, 1);
        assert!(
            snapshot
                .statuses
                .iter()
                .all(|s| matches!(s.state, CaptureState::Failed(_)))
        );
        service.reconcile(2, targets.clone(), false);
        assert_eq!(state.borrow().starts.len(), 1);
        service.reconcile(3, targets, true);
        assert_eq!(state.borrow().starts.len(), 2);
        state.borrow().publishers[1].publish(7);
        assert!(
            service
                .poll()
                .statuses
                .iter()
                .all(|s| matches!(s.state, CaptureState::Receiving { .. }))
        );
        service.shutdown();
    }

    #[test]
    fn start_failure_is_reported_for_all_dependents_without_reopening_device() {
        let state = std::rc::Rc::new(std::cell::RefCell::new(FakeState {
            fail_start: true,
            ..Default::default()
        }));
        let mut service = CaptureService::new(Backend(state.clone()));
        service.reconcile(1, vec![video(1920), video(1920)], false);
        assert_eq!(state.borrow().starts.len(), 1);
        assert!(
            service
                .poll()
                .statuses
                .iter()
                .all(|s| matches!(s.state, CaptureState::Failed(_)))
        );
        service.reconcile(2, Vec::new(), false);
        assert!(service.poll().statuses.is_empty());
    }

    #[test]
    fn audio_diagnostics_drain_bursts_and_surface_overflow() {
        let state = std::rc::Rc::new(std::cell::RefCell::new(FakeState::default()));
        let mut service = CaptureService::new(Backend(state.clone()));
        let audio = target(
            SourceKey::SystemAudio,
            CaptureFormat::Audio {
                sample_rate: 48_000,
                channels: 2,
            },
        );
        service.reconcile(1, vec![audio], false);
        for sample in 0..100 {
            state.borrow().publishers[0].publish(sample);
        }
        assert!(matches!(
            service.poll().statuses[0].state,
            CaptureState::Receiving {
                samples: 100,
                dropped: 0
            }
        ));
        for sample in 0..257 {
            state.borrow().publishers[0].publish(sample);
        }
        assert!(matches!(
            service.poll().statuses[0].state,
            CaptureState::Failed(CaptureError::Overload(_))
        ));
        assert_eq!(state.borrow().stops, 1);
    }
}
