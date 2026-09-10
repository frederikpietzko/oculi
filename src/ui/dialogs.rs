//! Native configuration sheets and application-wide settings, kept out of the workspace.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadOnly, sel};
use objc2_app_kit::*;

use super::{AppDelegate, label, ns, rect};

pub(super) struct SourceDialog {
    pub window: Retained<NSWindow>,
    pub name: Retained<NSTextField>,
    pub video: Retained<NSPopUpButton>,
    pub audio: Retained<NSPopUpButton>,
    pub error: Retained<NSTextField>,
    pub save: Retained<NSButton>,
    pub cancel: Retained<NSButton>,
}

pub(super) struct GlobalDialog {
    pub window: Retained<NSWindow>,
    pub guidance: Retained<NSTextField>,
}

fn window(delegate: &AppDelegate, title: &str, width: f64, height: f64) -> Retained<NSWindow> {
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(delegate.mtm()),
            rect(0.0, 0.0, width, height),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(&ns(title));
    window.setDelegate(Some(ProtocolObject::from_ref(delegate)));
    unsafe {
        window.setReleasedWhenClosed(false);
    }
    window
}

pub(super) fn source(delegate: &AppDelegate) -> SourceDialog {
    let mtm = delegate.mtm();
    let window = window(delegate, "Configure Source", 620.0, 442.0);
    let root = window.contentView().expect("source content view");
    let heading = label(
        &root,
        "One source. One recording.",
        rect(28.0, 388.0, 564.0, 28.0),
        mtm,
    );
    heading.setFont(Some(&NSFont::boldSystemFontOfSize(21.0)));
    let subtitle = label(
        &root,
        "Choose video and audio independently for this output.",
        rect(28.0, 362.0, 564.0, 22.0),
        mtm,
    );
    subtitle.setTextColor(Some(&NSColor::secondaryLabelColor()));
    label(&root, "Name", rect(28.0, 326.0, 564.0, 20.0), mtm);
    let name = NSTextField::textFieldWithString(&ns(""), mtm);
    name.setFrame(rect(28.0, 294.0, 564.0, 28.0));
    name.setPlaceholderString(Some(&ns("e.g. Desktop walkthrough")));
    root.addSubview(&name);
    label(&root, "VIDEO SOURCE", rect(28.0, 259.0, 564.0, 20.0), mtm);
    let video = NSPopUpButton::initWithFrame_pullsDown(
        NSPopUpButton::alloc(mtm),
        rect(24.0, 225.0, 572.0, 30.0),
        false,
    );
    label(&root, "AUDIO SOURCE", rect(28.0, 190.0, 564.0, 20.0), mtm);
    let audio = NSPopUpButton::initWithFrame_pullsDown(
        NSPopUpButton::alloc(mtm),
        rect(24.0, 156.0, 572.0, 30.0),
        false,
    );
    unsafe {
        video.setTarget(Some(delegate));
        video.setAction(Some(sel!(videoChanged:)));
        audio.setTarget(Some(delegate));
        audio.setAction(Some(sel!(audioChanged:)));
    }
    root.addSubview(&video);
    root.addSubview(&audio);
    let note = label(
        &root,
        "Application video uses one display. Application audio is app-wide.\nMissing devices? Open Oculi → Settings… to review permissions.",
        rect(28.0, 101.0, 564.0, 45.0),
        mtm,
    );
    note.setTextColor(Some(&NSColor::secondaryLabelColor()));
    note.setFont(Some(&NSFont::systemFontOfSize(11.0)));
    let error = label(&root, "", rect(28.0, 58.0, 564.0, 40.0), mtm);
    error.setTextColor(Some(&NSColor::systemRedColor()));
    let cancel = delegate.button(
        &root,
        "Cancel",
        rect(366.0, 18.0, 100.0, 32.0),
        sel!(cancelSource:),
    );
    cancel.setKeyEquivalent(&ns("\u{1b}"));
    let save = delegate.button(
        &root,
        "Add Source",
        rect(472.0, 18.0, 120.0, 32.0),
        sel!(saveSource:),
    );
    save.setKeyEquivalent(&ns("\r"));
    SourceDialog {
        window,
        name,
        video,
        audio,
        error,
        save,
        cancel,
    }
}

pub(super) fn global(delegate: &AppDelegate) -> GlobalDialog {
    let mtm = delegate.mtm();
    let window = window(delegate, "Oculi Settings", 640.0, 440.0);
    let root = window.contentView().expect("settings content view");
    let heading = label(
        &root,
        "Permissions & Discovery",
        rect(28.0, 386.0, 584.0, 28.0),
        mtm,
    );
    heading.setFont(Some(&NSFont::boldSystemFontOfSize(21.0)));
    let note = label(
        &root,
        "These permissions apply to all sources. Oculi never requests access automatically.",
        rect(28.0, 334.0, 584.0, 44.0),
        mtm,
    );
    note.setTextColor(Some(&NSColor::secondaryLabelColor()));
    for (title, x, width, action) in [
        ("Allow Camera", 28.0, 158.0, sel!(requestCamera:)),
        ("Allow Microphone", 194.0, 184.0, sel!(requestMicrophone:)),
        ("Allow Screen Capture", 386.0, 226.0, sel!(requestScreen:)),
    ] {
        delegate.button(&root, title, rect(x, 286.0, width, 34.0), action);
    }
    let guidance = label(&root, "", rect(28.0, 146.0, 584.0, 128.0), mtm);
    guidance.setFont(Some(&NSFont::systemFontOfSize(12.0)));
    delegate.button(
        &root,
        "Refresh Sources",
        rect(28.0, 99.0, 166.0, 32.0),
        sel!(refreshSources:),
    );
    let footer = label(
        &root,
        "Appearance follows macOS light / dark mode.\nOutput folders, filenames and formats are configured per recording in the bottom drawer.",
        rect(28.0, 28.0, 584.0, 56.0),
        mtm,
    );
    footer.setTextColor(Some(&NSColor::secondaryLabelColor()));
    GlobalDialog { window, guidance }
}
