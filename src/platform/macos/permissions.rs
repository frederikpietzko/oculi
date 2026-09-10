//! Permission prompts are explicit actions, never part of discovery refresh.
//! The app bundle must supply NSCameraUsageDescription and
//! NSMicrophoneUsageDescription before requesting access.

use block2::RcBlock;
use objc2::{rc::autoreleasepool, runtime::Bool};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio, AVMediaTypeVideo,
};
use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) fn camera_status() -> AVAuthorizationStatus {
    // SAFETY: AVMediaTypeVideo is a supported authorization query type.
    unsafe {
        AVMediaTypeVideo.map_or(AVAuthorizationStatus::Restricted, |media| {
            AVCaptureDevice::authorizationStatusForMediaType(media)
        })
    }
}

pub(super) fn microphone_status() -> AVAuthorizationStatus {
    // SAFETY: AVMediaTypeAudio is a supported authorization query type.
    unsafe {
        AVMediaTypeAudio.map_or(AVAuthorizationStatus::Restricted, |media| {
            AVCaptureDevice::authorizationStatusForMediaType(media)
        })
    }
}

pub(super) fn screen_granted() -> bool {
    CGPreflightScreenCaptureAccess()
}

/// Nonblocking; subsequent timer refreshes observe the user's decision.
pub fn request_camera() {
    autoreleasepool(|_| {
        // SAFETY: Reading the optional framework constant does not request access.
        let Some(media) = (unsafe { AVMediaTypeVideo }) else {
            log::warn!("Camera media type is unavailable; cannot request permission.");
            return;
        };
        let completion = RcBlock::new(|_: Bool| {});
        // SAFETY: Valid media type and a copied, capture-free completion block.
        unsafe {
            AVCaptureDevice::requestAccessForMediaType_completionHandler(media, &completion);
        }
    });
}

/// Nonblocking; subsequent timer refreshes observe the user's decision.
pub fn request_microphone() {
    autoreleasepool(|_| {
        // SAFETY: Reading the optional framework constant does not request access.
        let Some(media) = (unsafe { AVMediaTypeAudio }) else {
            log::warn!("Microphone media type is unavailable; cannot request permission.");
            return;
        };
        let completion = RcBlock::new(|_: Bool| {});
        // SAFETY: Valid media type and a copied, capture-free completion block.
        unsafe {
            AVCaptureDevice::requestAccessForMediaType_completionHandler(media, &completion);
        }
    });
}

/// Runs the potentially blocking system request off the UI thread. Repeated
/// clicks while the request is active are coalesced. A restart may be required.
pub fn request_screen() {
    static REQUESTING: AtomicBool = AtomicBool::new(false);
    if REQUESTING.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Err(error) = std::thread::Builder::new()
        .name("screen-permission".into())
        .spawn(|| {
            autoreleasepool(|_| {
                CGRequestScreenCaptureAccess();
            });
            REQUESTING.store(false, Ordering::Release);
        })
    {
        REQUESTING.store(false, Ordering::Release);
        log::warn!("Could not request screen permission: {error}");
    }
}

pub(super) fn status_label(status: AVAuthorizationStatus) -> &'static str {
    match status {
        AVAuthorizationStatus::Authorized => "granted",
        AVAuthorizationStatus::NotDetermined => "not requested",
        AVAuthorizationStatus::Denied => "denied",
        AVAuthorizationStatus::Restricted => "restricted by system policy",
        _ => "unknown",
    }
}

pub fn guidance() -> String {
    autoreleasepool(|_| {
        format!(
            "Camera: {}. Microphone: {}. Screen & System Audio Recording: {}. \
         Use the permission buttons to request access. For denied access, open \
         System Settings > Privacy & Security and enable Oculi under Camera, \
         Microphone, or Screen & System Audio Recording (Screen Recording on older macOS). \
         Restart Oculi if macOS asks, then refresh sources. Restricted access requires \
         an administrator. Restored windows must be manually reselected.",
            status_label(camera_status()),
            status_label(microphone_status()),
            if screen_granted() {
                "granted"
            } else {
                "not granted"
            },
        )
    })
}
