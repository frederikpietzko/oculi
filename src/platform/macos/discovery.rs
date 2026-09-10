//! One-shot, permission-aware discovery. Poll the returned receiver with
//! `try_recv` from the UI timer; do not wait on the main thread. Refresh again
//! only after completion, including after permission changes and hotplug.
//!
//! Poll `take_device_changes` for camera/microphone hotplug, debounce refreshes,
//! and retain the five-second screen refresh timer. Observers are installed once
//! on first refresh and retained on one parked thread for the process lifetime.
//! Each native subsystem has at most one outstanding operation, even after a
//! timeout. A stuck framework request cannot be cancelled; subsequent refreshes
//! warn until it finishes (or the app restarts).
//! All Objective-C objects stay on their worker/callback thread. Only owned
//! Rust selectors, strings, and synchronization primitives cross threads.

use super::permissions;
use crate::discovery::{SourceCatalog, same_audio, same_video};
use crate::model::{
    AudioSourceSelector, BundleId, DeviceId, DisplayId, VideoSourceSelector, WindowId,
};
use block2::RcBlock;
use objc2::{
    available,
    rc::{Retained, autoreleasepool},
    runtime::ProtocolObject,
};
use objc2_app_kit::NSRunningApplication;
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureDeviceDiscoverySession, AVCaptureDevicePosition,
    AVCaptureDeviceTypeBuiltInWideAngleCamera, AVCaptureDeviceTypeContinuityCamera,
    AVCaptureDeviceTypeDeskViewCamera, AVCaptureDeviceTypeExternal, AVCaptureDeviceTypeMicrophone,
    AVCaptureDeviceWasConnectedNotification, AVCaptureDeviceWasDisconnectedNotification,
    AVMediaTypeAudio, AVMediaTypeVideo,
};
use objc2_foundation::{
    NSArray, NSError, NSNotification, NSNotificationCenter, NSObjectProtocol, NSString,
};
use objc2_screen_capture_kit::SCShareableContent;
use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once, mpsc};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(8);
static REFRESHING: AtomicBool = AtomicBool::new(false);
static DEVICES_BUSY: AtomicBool = AtomicBool::new(false);
static SCREEN_BUSY: AtomicBool = AtomicBool::new(false);
static DEVICE_CHANGES: AtomicBool = AtomicBool::new(false);
static DEVICE_OBSERVATION: Once = Once::new();

/// Atomically consumes coalesced camera/microphone connect/disconnect events.
/// Poll on the UI timer and debounce before refreshing. The first successful
/// observer registration also invalidates discovery to cover its startup race.
/// Does not initialize observers or request permissions; `refresh` initializes.
pub fn take_device_changes() -> bool {
    DEVICE_CHANGES.swap(false, Ordering::AcqRel)
}

struct DeviceObservers {
    center: Retained<NSNotificationCenter>,
    tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl DeviceObservers {
    fn new(center: Retained<NSNotificationCenter>, changed: &'static AtomicBool) -> Self {
        let callback = RcBlock::new(move |_: NonNull<NSNotification>| {
            changed.store(true, Ordering::Release);
        });
        // SAFETY: The callback only captures a static atomic and ignores the
        // native notification. With no queue, delivery is on the posting thread;
        // no Objective-C object is sent elsewhere. The center copies the block.
        let tokens = unsafe {
            [
                AVCaptureDeviceWasConnectedNotification,
                AVCaptureDeviceWasDisconnectedNotification,
            ]
            .into_iter()
            .map(|name| {
                center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &callback)
            })
            .collect()
        };
        Self { center, tokens }
    }
}

impl Drop for DeviceObservers {
    fn drop(&mut self) {
        for token in &self.tokens {
            // SAFETY: Each token was registered with this center and is retained.
            unsafe { self.center.removeObserver((**token).as_ref()) };
        }
    }
}

fn initialize_device_observation() {
    DEVICE_OBSERVATION.call_once(|| {
        if let Err(error) = std::thread::Builder::new()
            .name("device-observation".into())
            .spawn(|| {
                let _observers = autoreleasepool(|_| {
                    DeviceObservers::new(NSNotificationCenter::defaultCenter(), &DEVICE_CHANGES)
                });
                DEVICE_CHANGES.store(true, Ordering::Release);
                // Tokens and center never leave this thread. No run loop is
                // needed for synchronous notifications (queue: nil).
                loop {
                    std::thread::park();
                }
            })
        {
            log::warn!("Could not observe device changes; timer refresh remains active: {error}");
        }
    });
}

struct Permit(&'static AtomicBool);

impl Permit {
    fn acquire(busy: &'static AtomicBool) -> Option<Self> {
        busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(busy))
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn warning(message: impl Into<String>) -> SourceCatalog {
    SourceCatalog {
        warnings: vec![message.into()],
        ..SourceCatalog::default()
    }
}

/// Returns one catalog within eight seconds of worker scheduling, preserving
/// completed subsystem results on timeout. No permission prompts are issued.
/// An overlapping call returns a warning-only catalog immediately.
pub fn refresh() -> mpsc::Receiver<SourceCatalog> {
    initialize_device_observation();
    let (sender, receiver) = mpsc::sync_channel(1);
    let Some(permit) = Permit::acquire(&REFRESHING) else {
        let _ = sender.try_send(warning("Source refresh is already in progress."));
        return receiver;
    };
    let result_sender = sender.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("source-refresh".into())
        .spawn(move || {
            let catalog = catch_unwind(|| {
                let deadline = Instant::now() + TIMEOUT;
                let (parts, results) = mpsc::sync_channel(2);
                start_part(
                    "Device discovery",
                    &DEVICES_BUSY,
                    parts.clone(),
                    |sender, _permit| {
                        let _ = sender.try_send(devices());
                    },
                );
                start_part("Screen discovery", &SCREEN_BUSY, parts, start_screen);
                collect_parts(results, deadline)
            })
            .unwrap_or_else(|_| warning("Source discovery worker panicked."));
            drop(permit);
            let _ = result_sender.try_send(catalog);
        })
    {
        let _ = sender.try_send(warning(format!("Could not start source refresh: {error}")));
    }
    receiver
}

fn start_part(
    name: &'static str,
    busy: &'static AtomicBool,
    sender: mpsc::SyncSender<SourceCatalog>,
    work: impl FnOnce(mpsc::SyncSender<SourceCatalog>, Arc<Permit>) + Send + 'static,
) {
    let Some(permit) = Permit::acquire(busy) else {
        let _ = sender.try_send(warning(format!(
            "{name} is still pending from an earlier refresh; retry later or restart Oculi."
        )));
        return;
    };
    let failure_sender = sender.clone();
    if let Err(error) = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                autoreleasepool(|_| work(sender.clone(), Arc::new(permit)));
            }));
            if result.is_err() {
                let _ = sender.try_send(warning(format!("{name} panicked.")));
            }
        })
    {
        let _ = failure_sender.try_send(warning(format!("Could not start {name}: {error}")));
    }
}

fn collect_parts(receiver: mpsc::Receiver<SourceCatalog>, deadline: Instant) -> SourceCatalog {
    let mut catalog = SourceCatalog::default();
    for _ in 0..2 {
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(part) => {
                catalog.videos.extend(part.videos);
                catalog.audios.extend(part.audios);
                catalog.warnings.extend(part.warnings);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                catalog.warnings.push(
                    "Source discovery timed out; incomplete sources are unavailable. Retry later."
                        .into(),
                );
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                catalog
                    .warnings
                    .push("Source discovery ended without all results.".into());
                break;
            }
        }
    }
    // Native enumeration and callback ordering can vary between refreshes.
    catalog.videos.sort_by_cached_key(|v| format!("{v:?}"));
    catalog.audios.sort_by_cached_key(|a| format!("{a:?}"));
    catalog.warnings.sort();
    catalog
}

fn devices() -> SourceCatalog {
    let mut catalog = SourceCatalog::default();
    // SAFETY: These are valid discovery types for the guarded OS version. We
    // read metadata only, never create AVCaptureDeviceInput or start a session.
    // All retained devices, arrays, and strings are dropped on this worker.
    unsafe {
        for camera in [true, false] {
            let (status, media, name) = if camera {
                (permissions::camera_status(), AVMediaTypeVideo, "Camera")
            } else {
                (
                    permissions::microphone_status(),
                    AVMediaTypeAudio,
                    "Microphone",
                )
            };
            let Some(media) = media else {
                catalog.warnings.push(format!(
                    "{name} media type is unavailable on this system; omitted."
                ));
                continue;
            };
            if status != AVAuthorizationStatus::Authorized {
                catalog.warnings.push(format!(
                    "{name} permission is {}; use the permission action or System Settings > Privacy & Security.",
                    permissions::status_label(status),
                ));
                continue;
            }
            let types = if available!(macos = 14.0) {
                if camera {
                    vec![
                        AVCaptureDeviceTypeBuiltInWideAngleCamera,
                        AVCaptureDeviceTypeExternal,
                        AVCaptureDeviceTypeContinuityCamera,
                        AVCaptureDeviceTypeDeskViewCamera,
                    ]
                } else {
                    vec![AVCaptureDeviceTypeMicrophone, AVCaptureDeviceTypeExternal]
                }
            } else {
                // The renamed constants were introduced in macOS 14.
                #[allow(deprecated)]
                if camera {
                    vec![
                        AVCaptureDeviceTypeBuiltInWideAngleCamera,
                        objc2_av_foundation::AVCaptureDeviceTypeExternalUnknown,
                    ]
                } else {
                    vec![
                        objc2_av_foundation::AVCaptureDeviceTypeBuiltInMicrophone,
                        objc2_av_foundation::AVCaptureDeviceTypeExternalUnknown,
                    ]
                }
            };
            let session =
                AVCaptureDeviceDiscoverySession::discoverySessionWithDeviceTypes_mediaType_position(
                    &NSArray::from_slice(&types),
                    Some(media),
                    AVCaptureDevicePosition::Unspecified,
                );
            for device in session.devices().iter() {
                let device_id = DeviceId(device.uniqueID().to_string());
                let label = device.localizedName().to_string();
                if device_id.0.is_empty() {
                    catalog.warnings.push(format!(
                        "{name} {label} has no persistent device ID; omitted."
                    ));
                } else if camera {
                    let source = VideoSourceSelector::Camera { device_id, label };
                    if !catalog.videos.iter().any(|v| same_video(v, &source)) {
                        catalog.videos.push(source);
                    }
                } else {
                    let source = AudioSourceSelector::Microphone { device_id, label };
                    if !catalog.audios.iter().any(|a| same_audio(a, &source)) {
                        catalog.audios.push(source);
                    }
                }
            }
        }
    }
    catalog
}

fn start_screen(sender: mpsc::SyncSender<SourceCatalog>, permit: Arc<Permit>) {
    if !available!(macos = 12.3) {
        let _ = sender.try_send(warning("Screen discovery requires macOS 12.3 or newer."));
        return;
    }
    if !permissions::screen_granted() {
        let _ = sender.try_send(warning(
            "Screen & System Audio Recording permission is not granted. Use the screen permission action; camera and microphone discovery remain independent.",
        ));
        return;
    }
    let callback_permit = permit.clone();
    let completion = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // The captured permit survives the public timeout for as long as the
            // framework retains this callback. No Objective-C object is captured.
            let _keep_alive = &callback_permit;
            let result = catch_unwind(AssertUnwindSafe(|| {
                autoreleasepool(|_| {
                    // SAFETY: ScreenCaptureKit lends these nullable objects for the
                    // callback duration. Convert everything before returning/sending.
                    unsafe {
                        if let Some(error) = error.as_ref() {
                            warning(format!(
                                "ScreenCaptureKit: {} ({}:{})",
                                error.localizedDescription(),
                                error.domain(),
                                error.code()
                            ))
                        } else if let Some(content) = content.as_ref() {
                            screen_catalog(content)
                        } else {
                            warning("ScreenCaptureKit returned neither content nor an error.")
                        }
                    }
                })
            }))
            .unwrap_or_else(|_| warning("ScreenCaptureKit result conversion panicked."));
            let _ = sender.try_send(result);
        },
    );
    // SAFETY: The API copies the block. It captures only Send + Sync Rust data;
    // native pointers are borrowed only during its invocation, never sent.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, false, &completion,
        );
    }
}

#[derive(Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Rect {
    fn intersects(self, other: Self) -> bool {
        self.width > 0.0
            && self.height > 0.0
            && other.width > 0.0
            && other.height > 0.0
            && self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }
}

impl From<objc2_core_foundation::CGRect> for Rect {
    fn from(frame: objc2_core_foundation::CGRect) -> Self {
        Self {
            x: frame.origin.x,
            y: frame.origin.y,
            width: frame.size.width,
            height: frame.size.height,
        }
    }
}

struct Application {
    bundle: String,
    label: String,
    pid: i32,
}
struct Window {
    pid: i32,
    frame: Rect,
    on_screen: bool,
}

fn screen_catalog(content: &SCShareableContent) -> SourceCatalog {
    let mut catalog = SourceCatalog::default();
    let mut displays = Vec::new();
    let mut applications = Vec::new();
    let mut windows = Vec::new();
    let own_pid = std::process::id() as i32;
    // SAFETY: content and all child objects are read and released exclusively
    // inside the framework callback. Result metadata is wholly owned Rust data.
    unsafe {
        for display in content.displays().iter() {
            let id = DisplayId(display.displayID());
            displays.push((id, Rect::from(display.frame())));
            catalog.videos.push(VideoSourceSelector::Display {
                display_id: id,
                label: format!(
                    "Display {} ({} × {})",
                    id.0,
                    display.width(),
                    display.height()
                ),
            });
        }
        for app in content.applications().iter() {
            applications.push(Application {
                bundle: app.bundleIdentifier().to_string(),
                label: app.applicationName().to_string(),
                pid: app.processID(),
            });
        }
        for window in content.windows().iter() {
            let owner = window.owningApplication();
            let pid = owner.as_ref().map(|a| a.processID()).unwrap_or(0);
            let bundle = owner
                .as_ref()
                .map(|a| a.bundleIdentifier().to_string())
                .filter(|s| !s.is_empty());
            if let Some(owner) = owner {
                applications.push(Application {
                    bundle: owner.bundleIdentifier().to_string(),
                    label: owner.applicationName().to_string(),
                    pid,
                });
            }
            if pid == own_pid || window.windowLayer() != 0 {
                continue;
            }
            windows.push(Window {
                pid,
                frame: window.frame().into(),
                on_screen: window.isOnScreen(),
            });
            catalog.videos.push(VideoSourceSelector::Window {
                window_id: WindowId(window.windowID()),
                owner_bundle_id: bundle.map(BundleId),
                title: window
                    .title()
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| format!("Window {}", window.windowID())),
            });
        }
    }
    // Shareable content need not include another instance with no windows.
    // Supplement identity checks using the thread-safe running-app API, but
    // never advertise bundles that ScreenCaptureKit did not report.
    let bundles: BTreeSet<_> = applications.iter().map(|a| a.bundle.clone()).collect();
    for bundle in bundles.into_iter().filter(|b| !b.is_empty()) {
        for app in NSRunningApplication::runningApplicationsWithBundleIdentifier(
            &NSString::from_str(&bundle),
        )
        .iter()
        {
            if !app.isTerminated() {
                applications.push(Application {
                    bundle: bundle.clone(),
                    label: app
                        .localizedName()
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| bundle.clone()),
                    pid: app.processIdentifier(),
                });
            }
        }
    }
    add_applications(
        &mut catalog,
        applications,
        &displays,
        &windows,
        own_pid,
        available!(macos = 13.0),
    );
    catalog
}

fn add_applications(
    catalog: &mut SourceCatalog,
    applications: Vec<Application>,
    displays: &[(DisplayId, Rect)],
    windows: &[Window],
    own_pid: i32,
    audio_supported: bool,
) {
    let mut bundles: BTreeMap<String, BTreeMap<i32, String>> = BTreeMap::new();
    for app in applications {
        if app.bundle.is_empty() {
            catalog.warnings.push(format!(
                "Application {} has no bundle identifier; app-wide selection omitted.",
                app.label
            ));
            continue;
        }
        bundles
            .entry(app.bundle)
            .or_default()
            .insert(app.pid, app.label);
    }
    if audio_supported {
        catalog.audios.push(AudioSourceSelector::System);
    } else {
        catalog
            .warnings
            .push("System and application audio require macOS 13 or newer.".into());
    }
    for (bundle, instances) in bundles {
        if instances.len() != 1 {
            catalog.warnings.push(format!(
                "Application {bundle} has {} running instances; app-wide video/audio selection is ambiguous and unavailable.", instances.len(),
            ));
            continue;
        }
        let (pid, label) = instances.into_iter().next().expect("nonempty bundle group");
        if pid == own_pid {
            continue;
        }
        if audio_supported {
            // App audio is app-wide, including apps with no on-screen windows.
            catalog.audios.push(AudioSourceSelector::Application {
                bundle_id: BundleId(bundle.clone()),
                label: label.clone(),
            });
        }
        for (display_id, frame) in displays {
            if windows
                .iter()
                .any(|w| w.pid == pid && w.on_screen && w.frame.intersects(*frame))
            {
                catalog.videos.push(VideoSourceSelector::Application {
                    bundle_id: BundleId(bundle.clone()),
                    display_id: *display_id,
                    label: format!("{label} — Display {}", display_id.0),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_preserves_partial_results() {
        let (sender, receiver) = mpsc::sync_channel(2);
        sender
            .try_send(SourceCatalog {
                audios: vec![AudioSourceSelector::System],
                ..SourceCatalog::default()
            })
            .unwrap();
        let catalog = collect_parts(receiver, Instant::now());
        assert_eq!(catalog.audios, vec![AudioSourceSelector::System]);
        assert!(catalog.warnings.iter().any(|w| w.contains("timed out")));
    }

    #[test]
    fn missing_callback_is_reported() {
        let (sender, receiver) = mpsc::sync_channel(2);
        drop(sender);
        let catalog = collect_parts(receiver, Instant::now() + Duration::from_secs(1));
        assert!(
            catalog
                .warnings
                .iter()
                .any(|w| w.contains("without all results"))
        );
    }

    #[test]
    fn native_guard_survives_until_all_callback_owners_are_dropped() {
        static BUSY: AtomicBool = AtomicBool::new(false);
        let worker = Arc::new(Permit::acquire(&BUSY).unwrap());
        let callback = worker.clone();
        drop(worker);
        assert!(Permit::acquire(&BUSY).is_none());
        drop(callback);
        assert!(Permit::acquire(&BUSY).is_some());
    }

    #[test]
    fn ambiguous_bundles_are_omitted_and_app_audio_is_display_independent() {
        let app = |bundle: &str, pid| Application {
            bundle: bundle.into(),
            label: bundle.into(),
            pid,
        };
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        };
        let mut catalog = SourceCatalog::default();
        add_applications(
            &mut catalog,
            vec![
                app("ambiguous", 1),
                app("ambiguous", 2),
                app("unique", 3),
                app("unique", 3),
                app("headless", 4),
                app("own", 9),
            ],
            &[
                (DisplayId(1), rect),
                (DisplayId(2), Rect { x: 100.0, ..rect }),
            ],
            &[Window {
                pid: 3,
                frame: rect,
                on_screen: true,
            }],
            9,
            true,
        );
        assert_eq!(catalog.audios.len(), 3); // System, unique, headless.
        assert_eq!(catalog.videos.len(), 1);
        assert!(
            matches!(&catalog.videos[0], VideoSourceSelector::Application { bundle_id, display_id: DisplayId(1), .. } if bundle_id.0 == "unique")
        );
        assert!(catalog.warnings.iter().any(|w| w.contains("ambiguous")));
        assert!(!catalog.audios.iter().any(|a| matches!(a, AudioSourceSelector::Application { bundle_id, .. } if bundle_id.0 == "own" || bundle_id.0 == "ambiguous")));
    }

    #[test]
    fn device_notifications_are_retained_coalesced_and_removed_on_drop() {
        static CHANGED: AtomicBool = AtomicBool::new(false);
        let center = NSNotificationCenter::new();
        let observers = autoreleasepool(|_| DeviceObservers::new(center.clone(), &CHANGED));
        autoreleasepool(|_| {
            assert!(!CHANGED.swap(false, Ordering::AcqRel));
            // SAFETY: This isolated center only invokes our callback, which does
            // not inspect the notification object. No devices or TCC are used.
            unsafe {
                center.postNotificationName_object(&NSString::from_str("unrelated"), None);
                assert!(!CHANGED.swap(false, Ordering::AcqRel));
                center.postNotificationName_object(AVCaptureDeviceWasConnectedNotification, None);
                center.postNotificationName_object(AVCaptureDeviceWasConnectedNotification, None);
                assert!(CHANGED.swap(false, Ordering::AcqRel));
                assert!(!CHANGED.swap(false, Ordering::AcqRel));
                center
                    .postNotificationName_object(AVCaptureDeviceWasDisconnectedNotification, None);
                assert!(CHANGED.swap(false, Ordering::AcqRel));
                drop(observers);
                center.postNotificationName_object(AVCaptureDeviceWasConnectedNotification, None);
                center
                    .postNotificationName_object(AVCaptureDeviceWasDisconnectedNotification, None);
                assert!(!CHANGED.swap(false, Ordering::AcqRel));
            }
        });
    }

    #[test]
    fn native_discovery_smoke() {
        if std::env::var("OCULI_NATIVE_DISCOVERY_SMOKE").as_deref() != Ok("1") {
            eprintln!(
                "SKIPPED native discovery smoke: set OCULI_NATIVE_DISCOVERY_SMOKE=1 with existing macOS permissions."
            );
            return;
        }
        if !permissions::screen_granted()
            || permissions::camera_status() != AVAuthorizationStatus::Authorized
            || permissions::microphone_status() != AVAuthorizationStatus::Authorized
        {
            eprintln!(
                "SKIPPED native discovery smoke: camera, microphone and screen permissions must already be granted; no prompts are issued."
            );
            return;
        }
        let catalog = refresh()
            .recv_timeout(TIMEOUT + Duration::from_secs(2))
            .expect("bounded discovery result");
        for warning in &catalog.warnings {
            eprintln!("Native discovery warning: {warning}");
        }
        assert!(
            !catalog.warnings.iter().any(|w| w.contains("timed out")
                || w.contains("panicked")
                || w.contains("Could not start")
                || w.contains("without all results")
                || w.contains("still pending")),
            "{:?}",
            catalog.warnings
        );
        assert!(
            catalog
                .videos
                .iter()
                .any(|v| matches!(v, VideoSourceSelector::Display { .. }))
        );
        if available!(macos = 13.0) {
            assert!(catalog.audios.contains(&AudioSourceSelector::System));
        }
        eprintln!(
            "Native discovery: {} video and {} audio sources.",
            catalog.videos.len(),
            catalog.audios.len()
        );
    }
}
