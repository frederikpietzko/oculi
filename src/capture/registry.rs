//! Worker-owned native sessions with nonblocking, bounded subscriber delivery.

use std::collections::{HashMap, VecDeque};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::{
    CaptureBackend, CaptureError, CaptureFormat, CaptureRequest, CaptureSession, SourceKey,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionPolicy {
    Latest,
    Bounded { capacity: usize, audio: bool },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubscriptionStats {
    /// Samples successfully enqueued (including subsequently replaced samples).
    pub received: u64,
    /// Queue evictions, rejected samples, and native/fanout drops since registration.
    pub dropped: u64,
    /// Zero if the queue is currently busy; querying stats never waits for delivery.
    pub queued: usize,
}

// The first error is immutable and retained until the enclosing shared state dies.
// Unlike OnceLock::set, competing callback failures never wait for initialization.
#[derive(Default)]
struct TerminalError(AtomicPtr<CaptureError>);

impl TerminalError {
    fn set(&self, error: CaptureError) {
        let error = Box::into_raw(Box::new(error));
        if self
            .0
            .compare_exchange(ptr::null_mut(), error, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // SAFETY: publication failed, so this allocation is still exclusively ours.
            unsafe { drop(Box::from_raw(error)) };
        }
    }

    fn get(&self) -> Option<CaptureError> {
        let error = self.0.load(Ordering::Acquire);
        // SAFETY: a published error is immutable and only freed in Drop, which cannot
        // run while this shared reference exists. Acquire observes its initialization.
        unsafe { error.as_ref() }.cloned()
    }

    fn is_set(&self) -> bool {
        !self.0.load(Ordering::Acquire).is_null()
    }
}

impl Drop for TerminalError {
    fn drop(&mut self) {
        let error = *self.0.get_mut();
        if !error.is_null() {
            // SAFETY: Drop has exclusive access and this is the single published Box.
            unsafe { drop(Box::from_raw(error)) };
        }
    }
}

struct Subscriber<T> {
    alive: AtomicBool,
    policy: SubscriptionPolicy,
    queue: Mutex<VecDeque<T>>,
    error: TerminalError,
    received: AtomicU64,
    dropped: AtomicU64,
    initial_shared_drops: u64,
}

impl<T> Subscriber<T> {
    fn drop_sample(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        if matches!(self.policy, SubscriptionPolicy::Bounded { audio: true, .. }) {
            self.error.set(CaptureError::Overload(
                "Audio subscriber could not accept a sample".into(),
            ));
        }
    }
}

struct Delivery<T> {
    active: AtomicBool,
    audio: bool,
    error: TerminalError,
    dropped: AtomicU64,
    // Only registration/reaping changes membership. Callbacks briefly try-lock to
    // clone a stable snapshot, then release this lock before touching any queues.
    subscribers: Mutex<Arc<Vec<Arc<Subscriber<T>>>>>,
}

impl<T> Delivery<T> {
    fn note_drop(&self) {
        if !self.active.load(Ordering::Acquire) || self.error.is_set() {
            return;
        }
        self.dropped.fetch_add(1, Ordering::Relaxed);
        if self.audio {
            self.error.set(CaptureError::Overload(
                "Audio capture dropped a sample before delivery".into(),
            ));
        }
    }

    fn retire(&self, error: CaptureError) {
        self.error.set(error);
        self.active.store(false, Ordering::Release);
    }
}

/// A callback-safe handle. It never owns or stops a native session.
pub struct SamplePublisher<T: Clone + Send + 'static> {
    delivery: Arc<Delivery<T>>,
}

impl<T: Clone + Send + 'static> Clone for SamplePublisher<T> {
    fn clone(&self) -> Self {
        Self {
            delivery: self.delivery.clone(),
        }
    }
}

impl<T: Clone + Send + 'static> SamplePublisher<T> {
    pub fn publish(&self, sample: T) {
        let delivery = &self.delivery;
        if !delivery.active.load(Ordering::Acquire) || delivery.error.is_set() {
            return;
        }
        let subscribers = match delivery.subscribers.try_lock() {
            Ok(subscribers) => subscribers.clone(),
            Err(_) => {
                delivery.note_drop();
                return;
            }
        };
        for subscriber in subscribers.iter() {
            if !subscriber.alive.load(Ordering::Acquire) || subscriber.error.is_set() {
                continue;
            }
            let Ok(mut queue) = subscriber.queue.try_lock() else {
                subscriber.drop_sample();
                continue;
            };
            if !delivery.active.load(Ordering::Acquire) || delivery.error.is_set() {
                return;
            }
            match subscriber.policy {
                SubscriptionPolicy::Latest => {
                    if queue.pop_front().is_some() {
                        subscriber.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                SubscriptionPolicy::Bounded { capacity, .. } if queue.len() >= capacity => {
                    subscriber.drop_sample();
                    continue;
                }
                SubscriptionPolicy::Bounded { .. } => {}
            }
            queue.push_back(sample.clone());
            subscriber.received.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Terminal failure is out-of-band: even a full or locked queue observes it.
    pub fn fail(&self, error: CaptureError) {
        if self.delivery.active.load(Ordering::Acquire) {
            self.delivery.error.set(error);
        }
    }

    /// Records a native drop for every current subscriber; audio fails immediately.
    pub fn note_drop(&self) {
        self.delivery.note_drop();
    }
}

/// Dropping a lease only marks it dead. Call `CaptureRegistry::reap` on the worker
/// to detach subscribers and stop native sessions. Buffered data remains readable
/// after failure; consumers should also check `error` to detect terminal failure.
pub struct CaptureLease<T: Clone + Send + 'static> {
    subscriber: Arc<Subscriber<T>>,
    delivery: Arc<Delivery<T>>,
}

impl<T: Clone + Send + 'static> CaptureLease<T> {
    pub fn try_recv(&self) -> Option<T> {
        self.subscriber.queue.try_lock().ok()?.pop_front()
    }

    pub fn error(&self) -> Option<CaptureError> {
        if !self.subscriber.error.is_set()
            && let Some(error) = self.delivery.error.get()
        {
            self.subscriber.error.set(error);
        }
        self.subscriber.error.get()
    }

    pub fn stats(&self) -> SubscriptionStats {
        SubscriptionStats {
            received: self.subscriber.received.load(Ordering::Relaxed),
            dropped: self.subscriber.dropped.load(Ordering::Relaxed)
                + self
                    .delivery
                    .dropped
                    .load(Ordering::Relaxed)
                    .saturating_sub(self.subscriber.initial_shared_drops),
            queued: self
                .subscriber
                .queue
                .try_lock()
                .map(|queue| queue.len())
                .unwrap_or(0),
        }
    }
}

impl<T: Clone + Send + 'static> Drop for CaptureLease<T> {
    fn drop(&mut self) {
        self.subscriber.alive.store(false, Ordering::Release);
    }
}

struct Session<T> {
    format: CaptureFormat,
    native: Box<dyn CaptureSession>,
    delivery: Arc<Delivery<T>>,
}

/// Every stop is attempted, and every error is returned with its source key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopErrors(pub Vec<(SourceKey, CaptureError)>);

impl std::fmt::Display for StopErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, (source, error)) in self.0.iter().enumerate() {
            if index > 0 {
                write!(f, "; ")?;
            }
            write!(f, "{source:?}: {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for StopErrors {}

/// Create, use, and drop on the capture worker. Native session objects are not Send.
pub struct CaptureRegistry<T: Clone + Send + 'static, B: CaptureBackend<T>> {
    backend: B,
    sessions: HashMap<SourceKey, Session<T>>,
    closed: bool,
}

impl<T: Clone + Send + 'static, B: CaptureBackend<T>> CaptureRegistry<T, B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            sessions: HashMap::new(),
            closed: false,
        }
    }

    pub fn subscribe(
        &mut self,
        request: CaptureRequest,
        policy: SubscriptionPolicy,
    ) -> Result<CaptureLease<T>, CaptureError> {
        if self.closed {
            return Err(CaptureError::Unavailable(
                "Capture registry is shut down".into(),
            ));
        }
        request.format.validate(&request.source)?;
        match policy {
            SubscriptionPolicy::Latest if request.source.is_audio() => {
                return Err(CaptureError::UnsupportedFormat(
                    "Audio requires a bounded audio subscription".into(),
                ));
            }
            SubscriptionPolicy::Bounded { capacity, audio }
                if capacity == 0 || audio != request.source.is_audio() =>
            {
                return Err(CaptureError::UnsupportedFormat(
                    "Subscription capacity must be positive and media type must match".into(),
                ));
            }
            _ => {}
        }
        if let Some(session) = self.sessions.get(&request.source) {
            if let Some(error) = session.delivery.error.get() {
                return Err(error);
            }
            if !session.format.supports(request.format) {
                return Err(CaptureError::UnsupportedFormat(
                    "Active capture cannot satisfy this format; reconcile demand before subscribing"
                        .into(),
                ));
            }
            let lease = Self::register(session.delivery.clone(), policy);
            return match lease.error() {
                Some(error) => Err(error),
                None => Ok(lease),
            };
        }

        let delivery = Arc::new(Delivery {
            active: AtomicBool::new(true),
            audio: request.source.is_audio(),
            error: TerminalError::default(),
            dropped: AtomicU64::new(0),
            subscribers: Mutex::new(Arc::new(Vec::new())),
        });
        // Register before start: a backend may deliver synchronously during startup.
        let lease = Self::register(delivery.clone(), policy);
        let publisher = SamplePublisher {
            delivery: delivery.clone(),
        };
        let native = match self.backend.start(&request, publisher) {
            Ok(native) => native,
            Err(error) => {
                delivery.retire(error.clone());
                return Err(error);
            }
        };
        self.sessions.insert(
            request.source,
            Session {
                format: request.format,
                native,
                delivery,
            },
        );
        if let Some(error) = lease.error() {
            return Err(error);
        }
        Ok(lease)
    }

    fn register(delivery: Arc<Delivery<T>>, policy: SubscriptionPolicy) -> CaptureLease<T> {
        let mut subscribers = delivery
            .subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let subscriber = Arc::new(Subscriber {
            alive: AtomicBool::new(true),
            policy,
            queue: Mutex::new(VecDeque::new()),
            error: TerminalError::default(),
            received: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            initial_shared_drops: delivery.dropped.load(Ordering::Relaxed),
        });
        let mut members: Vec<_> = subscribers
            .iter()
            .filter(|member| member.alive.load(Ordering::Acquire))
            .cloned()
            .collect();
        members.push(subscriber.clone());
        *subscribers = Arc::new(members);
        drop(subscribers);
        CaptureLease {
            subscriber,
            delivery,
        }
    }

    /// Removes unused and source-failed sessions, including failed sessions whose
    /// leases are still alive. Stop errors do not prevent the other stops.
    pub fn reap(&mut self) -> Result<(), StopErrors> {
        let sources: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|(source, session)| {
                let mut subscribers = session
                    .delivery
                    .subscribers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let members: Vec<_> = subscribers
                    .iter()
                    .filter(|member| member.alive.load(Ordering::Acquire))
                    .cloned()
                    .collect();
                let unused = members.is_empty();
                *subscribers = Arc::new(members);
                (unused || session.delivery.error.is_set()).then(|| source.clone())
            })
            .collect();
        self.stop_sources(sources, "Capture session retired")
    }

    pub fn fail_source(&self, source: &SourceKey, error: CaptureError) {
        if let Some(session) = self.sessions.get(source) {
            session.delivery.error.set(error);
        }
    }

    /// Permanently closes this registry, invalidates all publishers, and stops all
    /// sessions. Call explicitly to observe stop errors; Drop only logs errors.
    pub fn shutdown(&mut self) -> Result<(), StopErrors> {
        self.closed = true;
        self.stop_sources(
            self.sessions.keys().cloned().collect(),
            "Capture registry shut down",
        )
    }

    fn stop_sources(&mut self, sources: Vec<SourceKey>, reason: &str) -> Result<(), StopErrors> {
        let mut errors = Vec::new();
        for source in sources {
            if let Some(mut session) = self.sessions.remove(&source) {
                session
                    .delivery
                    .retire(CaptureError::Unavailable(reason.into()));
                if let Err(error) = session.native.stop() {
                    errors.push((source, error));
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(StopErrors(errors))
        }
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    pub fn subscriber_count(&self, source: &SourceKey) -> usize {
        self.sessions
            .get(source)
            .map(|session| {
                session
                    .delivery
                    .subscribers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .filter(|member| member.alive.load(Ordering::Acquire))
                    .count()
            })
            .unwrap_or(0)
    }
}

impl<T: Clone + Send + 'static, B: CaptureBackend<T>> Drop for CaptureRegistry<T, B> {
    fn drop(&mut self) {
        if let Err(errors) = self.shutdown() {
            for (source, error) in errors.0 {
                log::error!("Failed to stop capture {source:?}: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::thread::{self, ThreadId};

    use super::*;

    #[derive(Default)]
    struct FakeState {
        starts: usize,
        stops: usize,
        stop_threads: Vec<ThreadId>,
        publishers: Vec<SamplePublisher<u32>>,
        start_error: Option<CaptureError>,
        stop_error: Option<CaptureError>,
        startup_sample: Option<u32>,
        startup_failure: Option<CaptureError>,
    }

    struct FakeBackend(Rc<RefCell<FakeState>>);
    struct FakeSession(Rc<RefCell<FakeState>>);

    impl CaptureBackend<u32> for FakeBackend {
        fn start(
            &mut self,
            _request: &CaptureRequest,
            publisher: SamplePublisher<u32>,
        ) -> Result<Box<dyn CaptureSession>, CaptureError> {
            let mut state = self.0.borrow_mut();
            state.starts += 1;
            state.publishers.push(publisher.clone());
            if let Some(error) = state.start_error.clone() {
                return Err(error);
            }
            if let Some(sample) = state.startup_sample {
                publisher.publish(sample);
            }
            if let Some(error) = state.startup_failure.clone() {
                publisher.fail(error);
            }
            Ok(Box::new(FakeSession(self.0.clone())))
        }
    }

    impl CaptureSession for FakeSession {
        fn stop(&mut self) -> Result<(), CaptureError> {
            let mut state = self.0.borrow_mut();
            state.stops += 1;
            state.stop_threads.push(thread::current().id());
            match state.stop_error.clone() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    fn registry() -> (CaptureRegistry<u32, FakeBackend>, Rc<RefCell<FakeState>>) {
        let state = Rc::new(RefCell::new(FakeState::default()));
        (CaptureRegistry::new(FakeBackend(state.clone())), state)
    }

    fn video() -> CaptureRequest {
        CaptureRequest {
            source: SourceKey::Camera("camera".into()),
            format: CaptureFormat::Video {
                width: 1920,
                height: 1080,
                fps: 60,
            },
        }
    }

    fn audio() -> CaptureRequest {
        CaptureRequest {
            source: SourceKey::Microphone("microphone".into()),
            format: CaptureFormat::Audio {
                sample_rate: 48_000,
                channels: 2,
            },
        }
    }

    fn bounded(capacity: usize, audio: bool) -> SubscriptionPolicy {
        SubscriptionPolicy::Bounded { capacity, audio }
    }

    #[test]
    fn identical_and_lower_video_formats_share_one_native_session() {
        let (mut registry, state) = registry();
        let first = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        let second = registry.subscribe(video(), bounded(2, false)).unwrap();
        let mut lower = video();
        lower.format = CaptureFormat::Video {
            width: 640,
            height: 480,
            fps: 30,
        };
        let third = registry
            .subscribe(lower, SubscriptionPolicy::Latest)
            .unwrap();
        assert_eq!(state.borrow().starts, 1);
        assert_eq!(registry.session_count(), 1);
        assert_eq!(registry.subscriber_count(&video().source), 3);
        state.borrow().publishers[0].publish(7);
        assert_eq!(first.try_recv(), Some(7));
        assert_eq!(second.try_recv(), Some(7));
        assert_eq!(third.try_recv(), Some(7));
    }

    #[test]
    fn incompatible_upgrade_is_rejected_without_changing_the_session() {
        let (mut registry, state) = registry();
        let lease = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        for format in [
            CaptureFormat::Video {
                width: 3840,
                height: 1080,
                fps: 60,
            },
            CaptureFormat::Video {
                width: 1920,
                height: 2160,
                fps: 60,
            },
            CaptureFormat::Video {
                width: 1920,
                height: 1080,
                fps: 120,
            },
        ] {
            let mut request = video();
            request.format = format;
            assert!(matches!(
                registry.subscribe(request, SubscriptionPolicy::Latest),
                Err(CaptureError::UnsupportedFormat(_))
            ));
        }
        assert_eq!(state.borrow().starts, 1);
        assert_eq!(state.borrow().stops, 0);
        assert_eq!(registry.subscriber_count(&video().source), 1);
        state.borrow().publishers[0].publish(1);
        assert_eq!(lease.try_recv(), Some(1));
    }

    #[test]
    fn audio_reuse_requires_exact_format() {
        let (mut registry, state) = registry();
        let _first = registry.subscribe(audio(), bounded(2, true)).unwrap();
        let _second = registry.subscribe(audio(), bounded(3, true)).unwrap();
        let mut mono = audio();
        mono.format = CaptureFormat::Audio {
            sample_rate: 48_000,
            channels: 1,
        };
        assert!(matches!(
            registry.subscribe(mono, bounded(2, true)),
            Err(CaptureError::UnsupportedFormat(_))
        ));
        assert_eq!(state.borrow().starts, 1);
    }

    #[test]
    fn last_drop_on_another_thread_defers_stop_to_worker_reap() {
        let (mut registry, state) = registry();
        let first = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        let second = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        drop(first);
        registry.reap().unwrap();
        assert_eq!(state.borrow().stops, 0);
        assert_eq!(registry.subscriber_count(&video().source), 1);
        thread::spawn(move || drop(second)).join().unwrap();
        assert_eq!(state.borrow().stops, 0);
        assert_eq!(registry.subscriber_count(&video().source), 0);
        registry.reap().unwrap();
        registry.reap().unwrap();
        assert_eq!(registry.session_count(), 0);
        assert_eq!(state.borrow().stops, 1);
        assert_eq!(state.borrow().stop_threads, vec![thread::current().id()]);
    }

    #[test]
    fn source_failure_fans_out_and_rejects_late_subscribers_until_reaped() {
        let (mut registry, state) = registry();
        let first = registry.subscribe(video(), bounded(1, false)).unwrap();
        let second = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        let old = state.borrow().publishers[0].clone();
        old.publish(1);
        let failure = CaptureError::Unavailable("Camera disconnected".into());
        registry.fail_source(&video().source, failure.clone());
        old.fail(CaptureError::Native("Later failure".into()));
        assert_eq!(first.error(), Some(failure.clone()));
        assert_eq!(second.error(), Some(failure.clone()));
        assert!(
            matches!(registry.subscribe(video(), SubscriptionPolicy::Latest),
            Err(error) if error == failure)
        );
        assert_eq!(state.borrow().starts, 1);
        registry.reap().unwrap();
        assert_eq!(state.borrow().stops, 1);
        let restarted = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        old.publish(99);
        old.note_drop();
        old.fail(CaptureError::Native("Stale failure".into()));
        assert_eq!(restarted.try_recv(), None);
        assert_eq!(restarted.error(), None);
        assert_eq!(restarted.stats(), SubscriptionStats::default());
        state.borrow().publishers[1].publish(2);
        assert_eq!(restarted.try_recv(), Some(2));
        assert_eq!(first.error(), Some(failure));
        assert_eq!(first.try_recv(), Some(1));
        assert_eq!(first.try_recv(), None);
    }

    #[test]
    fn latest_replaces_queued_sample_and_counts_every_replacement() {
        let (mut registry, state) = registry();
        let lease = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        let publisher = state.borrow().publishers[0].clone();
        for sample in 1..=10 {
            publisher.publish(sample);
        }
        assert_eq!(
            lease.stats(),
            SubscriptionStats {
                received: 10,
                dropped: 9,
                queued: 1
            }
        );
        assert_eq!(lease.try_recv(), Some(10));
        assert_eq!(lease.try_recv(), None);
        assert_eq!(lease.error(), None);
    }

    #[test]
    fn bounded_video_drops_new_samples_without_affecting_fast_consumer() {
        let (mut registry, state) = registry();
        let slow = registry.subscribe(video(), bounded(2, false)).unwrap();
        let fast = registry.subscribe(video(), bounded(2, false)).unwrap();
        let publisher = state.borrow().publishers[0].clone();
        for sample in 0..10 {
            publisher.publish(sample);
            assert_eq!(fast.try_recv(), Some(sample));
        }
        assert_eq!(
            slow.stats(),
            SubscriptionStats {
                received: 2,
                dropped: 8,
                queued: 2
            }
        );
        assert_eq!(slow.try_recv(), Some(0));
        assert_eq!(slow.try_recv(), Some(1));
        assert_eq!(slow.error(), None);
        assert_eq!(
            fast.stats(),
            SubscriptionStats {
                received: 10,
                dropped: 0,
                queued: 0
            }
        );
    }

    #[test]
    fn bounded_audio_overflow_is_sticky_and_isolated_to_slow_consumer() {
        let (mut registry, state) = registry();
        let slow = registry.subscribe(audio(), bounded(1, true)).unwrap();
        let fast = registry.subscribe(audio(), bounded(1, true)).unwrap();
        let publisher = state.borrow().publishers[0].clone();
        publisher.publish(1);
        assert_eq!(fast.try_recv(), Some(1));
        publisher.publish(2);
        let failure = slow.error();
        assert!(matches!(failure, Some(CaptureError::Overload(_))));
        assert_eq!(
            slow.stats(),
            SubscriptionStats {
                received: 1,
                dropped: 1,
                queued: 1
            }
        );
        assert_eq!(slow.try_recv(), Some(1));
        assert_eq!(fast.try_recv(), Some(2));
        publisher.publish(3);
        assert_eq!(slow.try_recv(), None);
        assert_eq!(slow.error(), failure);
        assert_eq!(fast.try_recv(), Some(3));
        assert_eq!(fast.error(), None);
    }

    #[test]
    fn receiver_contention_drops_video_and_does_not_block_other_subscribers() {
        let (mut registry, state) = registry();
        let slow = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        let fast = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        let publisher = state.borrow().publishers[0].clone();
        let guard = slow.subscriber.queue.lock().unwrap();
        publisher.publish(42);
        assert_eq!(slow.try_recv(), None);
        assert_eq!(slow.stats().dropped, 1);
        assert_eq!(slow.error(), None);
        assert_eq!(fast.try_recv(), Some(42));
        drop(guard);
        publisher.publish(43);
        assert_eq!(slow.try_recv(), Some(43));
    }

    #[test]
    fn receiver_contention_fails_audio_out_of_band_without_blocking() {
        let (mut registry, state) = registry();
        let lease = registry.subscribe(audio(), bounded(1, true)).unwrap();
        let guard = lease.subscriber.queue.lock().unwrap();
        state.borrow().publishers[0].publish(42);
        assert!(matches!(lease.error(), Some(CaptureError::Overload(_))));
        assert_eq!(lease.stats().dropped, 1);
        drop(guard);
        assert_eq!(lease.try_recv(), None);
    }

    #[test]
    fn membership_contention_counts_video_drops_for_current_subscribers() {
        let (mut registry, state) = registry();
        let first = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        let second = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        let publisher = state.borrow().publishers[0].clone();
        let guard = publisher.delivery.subscribers.lock().unwrap();
        publisher.publish(1);
        assert_eq!(first.stats().dropped, 1);
        assert_eq!(second.stats().dropped, 1);
        assert_eq!(first.try_recv(), None);
        drop(guard);
        let late = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        assert_eq!(late.stats().dropped, 0);
        publisher.note_drop();
        assert_eq!(first.stats().dropped, 2);
        assert_eq!(late.stats().dropped, 1);
        publisher.publish(2);
        assert_eq!(first.try_recv(), Some(2));
        assert_eq!(late.try_recv(), Some(2));
    }

    #[test]
    fn membership_contention_fails_all_audio_subscribers() {
        let (mut registry, state) = registry();
        let first = registry.subscribe(audio(), bounded(1, true)).unwrap();
        let second = registry.subscribe(audio(), bounded(1, true)).unwrap();
        let publisher = state.borrow().publishers[0].clone();
        let _guard = publisher.delivery.subscribers.lock().unwrap();
        publisher.publish(1);
        assert!(matches!(first.error(), Some(CaptureError::Overload(_))));
        assert_eq!(second.error(), first.error());
        assert_eq!(first.stats().dropped, 1);
        assert_eq!(second.stats().dropped, 1);
    }

    #[test]
    fn native_audio_drop_fails_all_leases_and_full_queues_cannot_hide_failure() {
        let (mut registry, state) = registry();
        let first = registry.subscribe(audio(), bounded(1, true)).unwrap();
        let second = registry.subscribe(audio(), bounded(1, true)).unwrap();
        let publisher = state.borrow().publishers[0].clone();
        publisher.publish(1);
        let _guard = first.subscriber.queue.lock().unwrap();
        publisher.note_drop();
        assert!(matches!(first.error(), Some(CaptureError::Overload(_))));
        assert_eq!(second.error(), first.error());
        assert_eq!(first.stats().dropped, 1);
        assert_eq!(second.stats().queued, 1);
    }

    #[test]
    fn callback_failure_never_waits_for_membership_or_receiver_locks() {
        let (mut registry, state) = registry();
        let lease = registry.subscribe(video(), bounded(1, false)).unwrap();
        let publisher = state.borrow().publishers[0].clone();
        publisher.publish(1);
        let _members = publisher.delivery.subscribers.lock().unwrap();
        let _queue = lease.subscriber.queue.lock().unwrap();
        let failure = CaptureError::Permission("Revoked".into());
        publisher.fail(failure.clone());
        assert_eq!(lease.error(), Some(failure));
    }

    #[test]
    fn shutdown_stops_all_sources_and_leaves_old_publishers_inert() {
        let (mut registry, state) = registry();
        let video_lease = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        let audio_lease = registry.subscribe(audio(), bounded(1, true)).unwrap();
        let publishers = state.borrow().publishers.clone();
        registry.shutdown().unwrap();
        assert_eq!(registry.session_count(), 0);
        assert_eq!(registry.subscriber_count(&video().source), 0);
        assert_eq!(state.borrow().stops, 2);
        assert!(matches!(
            video_lease.error(),
            Some(CaptureError::Unavailable(_))
        ));
        assert!(matches!(
            audio_lease.error(),
            Some(CaptureError::Unavailable(_))
        ));
        for publisher in publishers {
            publisher.publish(1);
            publisher.note_drop();
            publisher.fail(CaptureError::Native("Late failure".into()));
        }
        assert_eq!(video_lease.stats(), SubscriptionStats::default());
        assert_eq!(audio_lease.stats(), SubscriptionStats::default());
        assert!(matches!(
            registry.subscribe(video(), SubscriptionPolicy::Latest),
            Err(CaptureError::Unavailable(_))
        ));
        registry.shutdown().unwrap();
        assert_eq!(state.borrow().stops, 2);
    }

    #[test]
    fn shutdown_collects_every_stop_error_and_drop_does_not_stop_twice() {
        let (mut registry, state) = registry();
        let _video = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        let _audio = registry.subscribe(audio(), bounded(1, true)).unwrap();
        let failure = CaptureError::Native("Stop failed".into());
        state.borrow_mut().stop_error = Some(failure.clone());
        let errors = registry.shutdown().unwrap_err();
        assert_eq!(errors.0.len(), 2);
        assert!(errors.0.contains(&(video().source, failure.clone())));
        assert!(errors.0.contains(&(audio().source, failure)));
        assert!(errors.to_string().contains("Camera"));
        assert!(errors.to_string().contains("Microphone"));
        assert_eq!(registry.session_count(), 0);
        drop(registry);
        assert_eq!(state.borrow().stops, 2);
    }

    #[test]
    fn reap_surfaces_stop_errors_and_invalidates_the_old_publisher() {
        let (mut registry, state) = registry();
        let lease = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        let old = state.borrow().publishers[0].clone();
        let failure = CaptureError::Native("Stop failed".into());
        state.borrow_mut().stop_error = Some(failure.clone());
        drop(lease);
        assert_eq!(
            registry.reap(),
            Err(StopErrors(vec![(video().source, failure)]))
        );
        assert_eq!(registry.session_count(), 0);
        assert!(!old.delivery.active.load(Ordering::Acquire));
        registry.reap().unwrap();
        assert_eq!(state.borrow().stops, 1);
    }

    #[test]
    fn dropping_registry_stops_native_session_but_dropping_publisher_does_not() {
        let (mut registry, state) = registry();
        let lease = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        let publisher = state.borrow().publishers[0].clone();
        drop(publisher);
        assert_eq!(state.borrow().stops, 0);
        drop(registry);
        assert_eq!(state.borrow().stops, 1);
        assert!(matches!(lease.error(), Some(CaptureError::Unavailable(_))));
    }

    #[test]
    fn backend_start_failure_leaves_no_session_and_publisher_is_inert() {
        let (mut registry, state) = registry();
        let failure = CaptureError::Permission("Camera permission".into());
        state.borrow_mut().start_error = Some(failure.clone());
        assert!(
            matches!(registry.subscribe(video(), SubscriptionPolicy::Latest),
            Err(error) if error == failure)
        );
        assert_eq!(registry.session_count(), 0);
        assert_eq!(state.borrow().stops, 0);
        let old = state.borrow().publishers[0].clone();
        assert!(!old.delivery.active.load(Ordering::Acquire));
        state.borrow_mut().start_error = None;
        let lease = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        old.publish(1);
        old.fail(CaptureError::Native("Old startup".into()));
        assert_eq!(lease.try_recv(), None);
        assert_eq!(lease.error(), None);
    }

    #[test]
    fn synchronous_startup_sample_reaches_first_lease() {
        let (mut registry, state) = registry();
        state.borrow_mut().startup_sample = Some(42);
        let lease = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        assert_eq!(lease.try_recv(), Some(42));
    }

    #[test]
    fn synchronous_startup_failure_is_retained_until_reap() {
        let (mut registry, state) = registry();
        let failure = CaptureError::Native("Startup callback failed".into());
        state.borrow_mut().startup_failure = Some(failure.clone());
        assert!(
            matches!(registry.subscribe(video(), SubscriptionPolicy::Latest),
            Err(error) if error == failure)
        );
        assert!(
            matches!(registry.subscribe(video(), SubscriptionPolicy::Latest),
            Err(error) if error == failure)
        );
        assert_eq!(state.borrow().starts, 1);
        assert_eq!(registry.session_count(), 1);
        registry.reap().unwrap();
        assert_eq!(state.borrow().stops, 1);
    }

    #[test]
    fn invalid_formats_and_policies_never_start_native_capture() {
        let (mut registry, state) = registry();
        for (request, policy) in [
            (audio(), SubscriptionPolicy::Latest),
            (audio(), bounded(1, false)),
            (video(), bounded(1, true)),
            (video(), bounded(0, false)),
            (audio(), bounded(0, true)),
            (
                CaptureRequest {
                    source: video().source,
                    format: audio().format,
                },
                bounded(1, false),
            ),
            (
                CaptureRequest {
                    source: audio().source,
                    format: video().format,
                },
                bounded(1, true),
            ),
            (
                CaptureRequest {
                    source: video().source,
                    format: CaptureFormat::Video {
                        width: 0,
                        height: 1080,
                        fps: 30,
                    },
                },
                SubscriptionPolicy::Latest,
            ),
            (
                CaptureRequest {
                    source: audio().source,
                    format: CaptureFormat::Audio {
                        sample_rate: 44_100,
                        channels: 2,
                    },
                },
                bounded(1, true),
            ),
        ] {
            assert!(matches!(
                registry.subscribe(request, policy),
                Err(CaptureError::UnsupportedFormat(_))
            ));
        }
        assert_eq!(state.borrow().starts, 0);
        assert_eq!(registry.session_count(), 0);
    }

    #[test]
    fn publishers_and_leases_cross_threads_without_native_objects() {
        let (mut registry, state) = registry();
        let lease = registry.subscribe(video(), bounded(64, false)).unwrap();
        let publisher = state.borrow().publishers[0].clone();
        thread::spawn(move || {
            for sample in 0..64 {
                publisher.publish(sample);
            }
        })
        .join()
        .unwrap();
        thread::spawn(move || {
            for sample in 0..64 {
                assert_eq!(lease.try_recv(), Some(sample));
            }
            assert_eq!(
                lease.stats(),
                SubscriptionStats {
                    received: 64,
                    dropped: 0,
                    queued: 0
                }
            );
        })
        .join()
        .unwrap();
        registry.reap().unwrap();
        assert_eq!(state.borrow().stops, 1);
    }

    #[test]
    fn simultaneous_callback_failures_publish_one_sticky_error() {
        let (mut registry, state) = registry();
        let first = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        let second = registry
            .subscribe(video(), SubscriptionPolicy::Latest)
            .unwrap();
        let publisher = state.borrow().publishers[0].clone();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|index| {
                let publisher = publisher.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    publisher.fail(CaptureError::Native(index.to_string()));
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let error = first.error();
        assert!(matches!(error, Some(CaptureError::Native(_))));
        assert_eq!(second.error(), error);
        publisher.fail(CaptureError::Native("Too late".into()));
        assert_eq!(first.error(), error);
    }

    #[test]
    fn observed_source_failure_cannot_be_replaced_by_an_inflight_local_drop() {
        let (mut registry, state) = registry();
        let lease = registry.subscribe(audio(), bounded(1, true)).unwrap();
        let failure = CaptureError::Unavailable("Source lost".into());
        state.borrow().publishers[0].fail(failure.clone());
        assert_eq!(lease.error(), Some(failure.clone()));
        lease.subscriber.drop_sample();
        assert_eq!(lease.error(), Some(failure));
    }
}
