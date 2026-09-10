//! Main-thread-only AppKit configuration interface.

mod dialogs;
mod group;
mod source_picker;
mod viewport;
mod workspace;

use std::cell::{Cell, OnceCell, RefCell};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_foundation::{
    NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSTimer,
};

use crate::app::{AppState, SettingsWorker};
use crate::discovery::{SourceCatalog, same_audio, same_video};
use crate::model::{AudioSourceSelector, RecordingConfig, VideoSourceSelector};
use crate::platform::macos::capture::CaptureWorker;
use crate::platform::macos::{discovery, permissions};
use crate::storage::default_settings_path;
use source_picker::*;

struct Controls {
    workspace: workspace::Workspace,
    source: dialogs::SourceDialog,
    global: dialogs::GlobalDialog,
    framing: viewport::Framing,
    window: Retained<NSWindow>,
    name: Retained<NSTextField>,
    video: Retained<NSPopUpButton>,
    audio: Retained<NSPopUpButton>,
    directory: Retained<NSTextField>,
    template: Retained<NSTextField>,
    width: Retained<NSTextField>,
    height: Retained<NSTextField>,
    fps: Retained<NSTextField>,
    status: Retained<NSTextField>,
    guidance: Retained<NSTextField>,
    message: Retained<NSTextField>,
    edit_buttons: Vec<Retained<NSButton>>,
}

struct DelegateIvars {
    state: RefCell<AppState>,
    saver: RefCell<SettingsWorker>,
    capture: RefCell<CaptureWorker>,
    controls: OnceCell<Controls>,
    videos: RefCell<Vec<VideoSourceSelector>>,
    audios: RefCell<Vec<AudioSourceSelector>>,
    discovery: RefCell<Option<Receiver<SourceCatalog>>>,
    last_refresh: Cell<Instant>,
    last_slow_tick: Cell<Instant>,
    viewport_edited: Cell<Option<Instant>>,
    device_refresh_due: Cell<bool>,
    timer: RefCell<Option<Retained<NSTimer>>>,
    quitting: Cell<bool>,
    quit_after_group: Cell<bool>,
    quit_stop_sent: Cell<bool>,
    group_menu: OnceCell<Vec<Retained<NSMenuItem>>>,
    smoke_directory: Option<std::path::PathBuf>,
    smoke_started: Cell<bool>,
    drawer_open: Cell<bool>,
    source_draft: RefCell<Option<RecordingConfig>>,
    source_is_new: Cell<bool>,
    video_reselected: Cell<bool>,
    delete_alert: RefCell<Option<Retained<NSAlert>>>,
    smoke_phase: Cell<u32>,
    smoke_preview: RefCell<Option<(crate::model::RecordingId, crate::render::RenderedFrame)>>,
    smoke_preview_ticks: Cell<u32>,
    smoke_tracking_ticks: Cell<u32>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = DelegateIvars]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            self.build_window();
            self.refresh_editor();
            self.begin_refresh();
            if self.ivars().state.borrow().can_save {
                self.ivars().saver.borrow_mut().validate_restored(&self.ivars().state.borrow().settings);
            }
            // NSTimer invokes this selector on the main run loop, where self lives.
            let timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                1.0 / 30.0, self, sel!(tick:), None, true,
            ) };
            // TerminateLater enters a modal run loop; persistence/capture polling
            // must continue there until the asynchronous quit reply is ready.
            unsafe { objc2_foundation::NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSModalPanelRunLoopMode); }
            unsafe { objc2_foundation::NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSEventTrackingRunLoopMode); }
            *self.ivars().timer.borrow_mut() = Some(timer);
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn terminate_after_last_window(&self, _app: &NSApplication) -> bool { true }

        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _app: &NSApplication) -> NSApplicationTerminateReply {
            if self.ivars().quit_after_group.get() { return NSApplicationTerminateReply::TerminateLater; }
            if self.ivars().source_draft.borrow().is_some() {
                self.controls().source.window.makeKeyAndOrderFront(None);
                return NSApplicationTerminateReply::TerminateCancel;
            }
            self.flush_viewport_edit();
            if !self.commit_editor(false) { return NSApplicationTerminateReply::TerminateCancel; }
            self.poll_group();
            if self.ivars().state.borrow().configuration_locked() {
                let alert = NSAlert::new(self.mtm());
                alert.setMessageText(&ns("Stop & Save before quitting?"));
                alert.setInformativeText(&ns("Oculi will stay open and responsive until every recording finishes saving. Encoder finalization will not be interrupted."));
                alert.addButtonWithTitle(&ns("Cancel"));
                alert.addButtonWithTitle(&ns("Stop & Save, then Quit"));
                if alert.runModal() != NSAlertSecondButtonReturn { return NSApplicationTerminateReply::TerminateCancel; }
                self.ivars().quitting.set(true);
                self.ivars().quit_after_group.set(true);
                self.ivars().quit_stop_sent.set(false);
                self.save();
                return NSApplicationTerminateReply::TerminateLater;
            }
            self.save();
            self.ivars().capture.borrow().shutdown();
            if self.ivars().saver.borrow().pending || self.ivars().state.borrow().dirty || !self.ivars().capture.borrow().is_finished() {
                self.ivars().quitting.set(true);
                NSApplicationTerminateReply::TerminateLater
            } else { NSApplicationTerminateReply::TerminateNow }
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _notification: &NSNotification) {
            self.ivars().capture.borrow().shutdown();
            if let Some(timer) = self.ivars().timer.borrow_mut().take() { timer.invalidate(); }
        }
    }

    unsafe impl NSWindowDelegate for AppDelegate {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, window: &NSWindow) -> bool {
            if window == &*self.controls().framing.window { self.flush_viewport_edit(); }
            if window == &*self.controls().source.window {
                self.dismiss_source();
                return false.into();
            }
            if window != &*self.controls().window { return true.into(); }
            NSApplication::sharedApplication(self.mtm()).terminate(None);
            false
        }

        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, _notification: &NSNotification) {
            if let Some(c) = self.ivars().controls.get() {
                workspace::layout(self);
                c.framing.preview.layout();
            }
        }
    }

    impl AppDelegate {
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &NSTimer) {
            if !self.ivars().quitting.get() || self.ivars().quit_after_group.get() { self.poll_previews(); }
            if self.ivars().last_slow_tick.get().elapsed() < Duration::from_millis(200) { return; }
            self.ivars().last_slow_tick.set(Instant::now());
            let result = self.ivars().discovery.borrow().as_ref().map(|rx| rx.try_recv());
            match result {
                Some(Ok(catalog)) => {
                    self.ivars().discovery.borrow_mut().take();
                    let changed = self.ivars().state.borrow().catalog != catalog;
                    self.ivars().state.borrow_mut().update_catalog(catalog);
                    if changed { self.refresh_pickers(); self.refresh_list(); }
                }
                Some(Err(TryRecvError::Disconnected)) => {
                    self.ivars().discovery.borrow_mut().take();
                    self.ivars().state.borrow_mut().message = "Source discovery stopped. Use Refresh to retry.".into();
                }
                _ => {}
            }
            if discovery::take_device_changes() { self.ivars().device_refresh_due.set(true); }
            let elapsed = self.ivars().last_refresh.get().elapsed();
            if elapsed >= Duration::from_secs(5) || (self.ivars().device_refresh_due.get() && elapsed >= Duration::from_millis(500)) { self.begin_refresh(); }
            let result = self.ivars().saver.borrow().results.try_recv();
            if let Ok(result) = result {
                self.ivars().saver.borrow_mut().pending = false;
                let mut state = self.ivars().state.borrow_mut();
                if let Some(error) = result.error {
                    self.ivars().saver.borrow_mut().failed = true;
                    state.message = format!("Settings not saved: {error}. Check the settings directory and press Apply to retry.");
                    state.dirty = true;
                    if self.ivars().quitting.replace(false) {
                        self.ivars().quit_after_group.set(false);
                        NSApplication::sharedApplication(self.mtm()).replyToApplicationShouldTerminate(false);
                    }
                } else {
                    state.message = if result.destination_warnings.is_empty() {
                        if result.wrote_settings { "Configuration saved.".into() } else { String::new() }
                    } else { format!("Fix destinations before recording: {}", result.destination_warnings.join("; ")) };
                }
            }
            // New edits made during a save are sent once that save completes.
            if !self.ivars().saver.borrow().failed
                && self.ivars().viewport_edited.get().is_none_or(|time| time.elapsed() >= Duration::from_millis(400))
            {
                self.ivars().viewport_edited.set(None);
                self.save();
            }
            self.poll_capture();
            self.poll_group();
            self.advance_group_quit();
            if self.ivars().quitting.get() && !self.ivars().quit_after_group.get() && !self.ivars().saver.borrow().pending && !self.ivars().state.borrow().dirty && self.ivars().capture.borrow().is_finished() {
                NSApplication::sharedApplication(self.mtm()).replyToApplicationShouldTerminate(true);
            }
            self.refresh_status();
            if self.ivars().smoke_directory.is_some() && !self.ivars().smoke_started.get() { self.smoke_actions(); }
            if self.ivars().smoke_started.get() && !self.ivars().quitting.get() && !self.ivars().saver.borrow().pending && !self.ivars().state.borrow().dirty {
                println!("Oculi native UI smoke: PASS (source dialogs, cards, framing controls, independent edits, synthetic Metal presentation, tracking-mode ticks, fixed canvas, preview layers, lifecycle, output validation, settings window, resize, asynchronous save, group controls and real backend rejection)");
                self.ivars().quitting.set(true);
                // Do not nest termination inside this timer: NSTimer cannot fire
                // again until this invocation returns, including during a modal quit.
                dispatch2::DispatchQueue::main().exec_async(|| {
                    let mtm = MainThreadMarker::new().expect("main dispatch queue");
                    NSApplication::sharedApplication(mtm).terminate(None);
                });
            }
        }

        #[unsafe(method(addRecording:))]
        fn add_recording(&self, _sender: &NSButton) {
            if !self.commit_editor(false) { return; }
            let mut draft = RecordingConfig::new(format!("Recording {}", self.ivars().state.borrow().settings.recordings.len() + 1));
            if let Some(directory) = &self.ivars().smoke_directory { draft.output.directory = directory.join("outputs"); }
            self.open_source(draft, true);
        }

        #[unsafe(method(editRecording:))]
        fn edit_recording(&self, _sender: &NSButton) {
            if !self.commit_editor(false) { return; }
            let selected = self.ivars().state.borrow().selected().cloned();
            if let Some(draft) = selected { self.open_source(draft, false); }
        }

        #[unsafe(method(saveSource:))]
        fn save_source(&self, _sender: &NSButton) { self.commit_source(); }

        #[unsafe(method(cancelSource:))]
        fn cancel_source(&self, _sender: &NSButton) { self.dismiss_source(); }

        #[unsafe(method(showSettings:))]
        fn show_settings(&self, _sender: &NSObject) {
            self.refresh_status();
            self.controls().global.window.center();
            self.controls().global.window.makeKeyAndOrderFront(None);
        }

        #[unsafe(method(showFraming:))]
        fn show_framing(&self, _sender: &NSObject) {
            self.flush_viewport_edit();
            if !self.commit_editor(false) { return; }
            let state = self.ivars().state.borrow();
            if let Some(recording) = state.selected() {
                self.controls().framing.show(recording, state.can_save);
            }
        }

        #[unsafe(method(applyFraming:))]
        fn apply_framing(&self, _sender: &NSObject) { self.edit_framing("apply"); }

        #[unsafe(method(fitFraming:))]
        fn fit_framing(&self, _sender: &NSObject) { self.edit_framing("fit"); }

        #[unsafe(method(fillFraming:))]
        fn fill_framing(&self, _sender: &NSObject) { self.edit_framing("fill"); }

        #[unsafe(method(resetFraming:))]
        fn reset_framing(&self, _sender: &NSObject) { self.edit_framing("reset"); }

        #[unsafe(method(toggleDrawer:))]
        fn toggle_drawer(&self, _sender: &NSButton) {
            self.ivars().drawer_open.set(!self.ivars().drawer_open.get());
            workspace::layout(self);
        }

        #[unsafe(method(duplicateRecording:))]
        fn duplicate_recording(&self, _sender: &NSButton) {
            if !self.commit_editor(false) { return; }
            self.ivars().state.borrow_mut().duplicate();
            self.refresh_editor(); self.save();
        }

        #[unsafe(method(removeRecording:))]
        fn remove_recording(&self, _sender: &NSButton) {
            self.confirm_delete();
        }

        #[unsafe(method(selectRecording:))]
        fn select_recording(&self, sender: &NSButton) {
            if !self.commit_editor(false) { return; }
            let mut state = self.ivars().state.borrow_mut();
            state.selected = state.settings.recordings.get(sender.tag() as usize).map(|r| r.id);
            drop(state);
            self.refresh_editor(); self.save();
        }

        #[unsafe(method(applyChanges:))]
        fn apply_changes(&self, _sender: &NSObject) {
            if self.commit_editor(false) { self.refresh_list(); self.save(); }
        }

        #[unsafe(method(videoChanged:))]
        fn video_changed(&self, _sender: &NSPopUpButton) {
            self.ivars().video_reselected.set(true);
            self.read_source_selection();
        }

        #[unsafe(method(audioChanged:))]
        fn audio_changed(&self, _sender: &NSPopUpButton) {
            self.read_source_selection();
        }

        #[unsafe(method(chooseDirectory:))]
        fn choose_directory(&self, _sender: &NSButton) {
            if !self.ivars().state.borrow().configuration_editable() { return; }
            let panel = NSOpenPanel::openPanel(self.mtm());
            panel.setCanChooseDirectories(true);
            panel.setCanChooseFiles(false);
            panel.setCanCreateDirectories(true);
            panel.setAllowsMultipleSelection(false);
            // AppKit's modal run loop continues processing native events.
            if panel.runModal() == NSModalResponseOK
                && let Some(url) = panel.URL()
                && let Some(path) = url.path()
            {
                if !self.ivars().state.borrow().configuration_editable() { return; }
                self.controls().directory.setStringValue(&path);
                if self.commit_editor(false) { self.save(); }
            }
        }

        #[unsafe(method(refreshSources:))]
        fn refresh_sources(&self, _sender: &NSButton) {
            self.ivars().capture.borrow_mut().retry();
            self.begin_refresh();
        }

        #[unsafe(method(requestCamera:))]
        fn request_camera(&self, _sender: &NSButton) { permissions::request_camera(); }

        #[unsafe(method(requestMicrophone:))]
        fn request_microphone(&self, _sender: &NSButton) { permissions::request_microphone(); }

        #[unsafe(method(requestScreen:))]
        fn request_screen(&self, _sender: &NSButton) { permissions::request_screen(); }

        #[unsafe(method(startGroup:))]
        fn start_group(&self, _sender: &NSObject) { self.start_recordings(); }

        #[unsafe(method(pauseGroup:))]
        fn pause_group(&self, _sender: &NSObject) { self.pause_recordings(); }

        #[unsafe(method(stopGroup:))]
        fn stop_group(&self, _sender: &NSObject) { self.stop_recordings(); }
    }
);

impl AppDelegate {
    fn controls(&self) -> &Controls {
        self.ivars().controls.get().expect("window initialized")
    }

    fn new(mtm: MainThreadMarker, smoke_directory: Option<std::path::PathBuf>) -> Retained<Self> {
        let path = smoke_directory
            .as_ref()
            .map(|p| p.join("settings.json"))
            .unwrap_or_else(default_settings_path);
        let state = AppState::load(&path);
        let this = Self::alloc(mtm).set_ivars(DelegateIvars {
            state: RefCell::new(state),
            saver: RefCell::new(SettingsWorker::new(path)),
            capture: RefCell::new(CaptureWorker::new()),
            controls: OnceCell::new(),
            videos: RefCell::new(Vec::new()),
            audios: RefCell::new(Vec::new()),
            discovery: RefCell::new(None),
            last_refresh: Cell::new(Instant::now()),
            last_slow_tick: Cell::new(Instant::now()),
            viewport_edited: Cell::new(None),
            device_refresh_due: Cell::new(false),
            timer: RefCell::new(None),
            quitting: Cell::new(false),
            quit_after_group: Cell::new(false),
            quit_stop_sent: Cell::new(false),
            group_menu: OnceCell::new(),
            smoke_directory,
            smoke_started: Cell::new(false),
            drawer_open: Cell::new(true),
            source_draft: RefCell::new(None),
            source_is_new: Cell::new(false),
            video_reselected: Cell::new(false),
            delete_alert: RefCell::new(None),
            smoke_phase: Cell::new(0),
            smoke_preview: RefCell::new(None),
            smoke_preview_ticks: Cell::new(0),
            smoke_tracking_ticks: Cell::new(0),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn smoke_actions(&self) {
        let c = self.controls();
        if objc2_foundation::NSRunLoop::mainRunLoop()
            .currentMode()
            .as_deref()
            == Some(unsafe { NSEventTrackingRunLoopMode })
        {
            return;
        }
        if c.window.attachedSheet().is_some() {
            return;
        }
        match self.ivars().smoke_phase.get() {
            0 => {
                self.smoke_click(sel!(addRecording:));
                assert!(self.ivars().source_draft.borrow().is_some());
                assert!(self.ivars().state.borrow().settings.recordings.is_empty());
                c.name.setStringValue(&ns(" "));
                self.smoke_click(sel!(saveSource:));
                assert!(self.ivars().source_draft.borrow().is_some());
                assert!(!c.source.error.stringValue().is_empty());
                unsafe {
                    c.source.cancel.performClick(None);
                }
            }
            1 => {
                assert!(self.ivars().state.borrow().settings.recordings.is_empty());
                self.smoke_click(sel!(addRecording:));
                c.name.setStringValue(&ns("Smoke A"));
                unsafe {
                    c.source.save.performClick(None);
                }
            }
            2 => {
                assert_eq!(self.ivars().state.borrow().settings.recordings.len(), 1);
                c.width.setStringValue(&ns("1921"));
                assert!(!self.commit_editor(false), "invalid output was accepted");
                c.width.setStringValue(&ns("1920"));
                self.smoke_click(sel!(applyChanges:));
                self.smoke_click(sel!(editRecording:));
                c.name.setStringValue(&ns("Canceled edit"));
                self.smoke_click(sel!(cancelSource:));
            }
            3 => {
                assert_eq!(
                    self.ivars().state.borrow().selected().unwrap().name,
                    "Smoke A"
                );
                self.smoke_click(sel!(editRecording:));
                c.name.setStringValue(&ns("Smoke A edited"));
                self.refresh_pickers();
                assert_eq!(c.name.stringValue().to_string(), "Smoke A edited");
                self.smoke_click(sel!(saveSource:));
            }
            4 => {
                assert_eq!(
                    self.ivars().state.borrow().selected().unwrap().name,
                    "Smoke A edited"
                );
                self.smoke_click(sel!(duplicateRecording:));
                assert_eq!(self.ivars().state.borrow().settings.recordings.len(), 2);
                self.smoke_click(sel!(removeRecording:));
                let alert = self.ivars().delete_alert.borrow().as_ref().unwrap().clone();
                // Respond through AppKit's sheet lifecycle, not the state mutation helper.
                c.window
                    .endSheet_returnCode(&alert.window(), NSAlertFirstButtonReturn);
            }
            5 => {
                assert_eq!(
                    self.ivars().state.borrow().settings.recordings.len(),
                    2,
                    "Cancel deleted a source"
                );
                self.smoke_click(sel!(removeRecording:));
                let alert = self.ivars().delete_alert.borrow().as_ref().unwrap().clone();
                c.window
                    .endSheet_returnCode(&alert.window(), NSAlertSecondButtonReturn);
            }
            6 => {
                assert_eq!(self.ivars().state.borrow().settings.recordings.len(), 1);
                self.smoke_click(sel!(duplicateRecording:));
                let first = self.ivars().state.borrow().settings.recordings[0].id;
                let mut selected = false;
                for card in c.workspace.grid.subviews().iter() {
                    for child in card.subviews().iter() {
                        if let Some(button) = child.downcast_ref::<NSButton>()
                            && button.tag() == 0
                        {
                            unsafe {
                                button.performClick(None);
                            }
                            selected = true;
                        }
                    }
                }
                assert!(selected, "First preview card has no selection control");
                assert_eq!(self.ivars().state.borrow().selected, Some(first));
                assert!(
                    c.workspace
                        .group_buttons
                        .iter()
                        .all(|button| !button.isEnabled())
                );
                self.smoke_click(sel!(startGroup:));
                assert!(!self.ivars().state.borrow().configuration_locked());
                assert!(
                    self.ivars()
                        .state
                        .borrow()
                        .message
                        .contains("available video and audio")
                );
                let before = self.ivars().state.borrow().settings.recordings.clone();
                self.smoke_click(sel!(pauseGroup:));
                self.smoke_click(sel!(stopGroup:));
                assert_eq!(self.ivars().state.borrow().settings.recordings, before);
                self.smoke_click(sel!(toggleDrawer:));
                assert!(!self.ivars().drawer_open.get());
                self.smoke_click(sel!(toggleDrawer:));
                c.window.setContentSize(NSSize::new(1440.0, 900.0));
                workspace::layout(self);
                assert!(c.workspace.grid.subviews().count() >= 2);
                c.window.setContentSize(NSSize::new(1120.0, 800.0));
                workspace::layout(self);
                let menu = NSApplication::sharedApplication(self.mtm())
                    .mainMenu()
                    .unwrap();
                menu.itemAtIndex(0)
                    .unwrap()
                    .submenu()
                    .unwrap()
                    .performActionForItemAtIndex(0);
                assert!(
                    c.global.window.isVisible(),
                    "Settings menu did not open its window"
                );
                c.global.window.performClose(None);
                assert!(
                    c.window.isVisible(),
                    "Closing settings closed the workspace"
                );
                self.smoke_framing();
                self.save_smoke_image("workspace.png");
                let appearance = NSAppearance::appearanceNamed(&ns("NSAppearanceNameAqua"))
                    .expect("light appearance");
                c.window.setAppearance(Some(&appearance));
            }
            7 => {
                self.save_smoke_image("workspace-light.png");
                let appearance = NSAppearance::appearanceNamed(&ns("NSAppearanceNameDarkAqua"))
                    .expect("dark appearance");
                c.window.setAppearance(Some(&appearance));
            }
            8 => {
                self.save_smoke_image("workspace-dark.png");
                c.window.setAppearance(None);
                self.smoke_start_presentation();
            }
            9 => {
                // Occlusion state arrives asynchronously after showing the window.
                if !c.framing.preview.visible() {
                    assert!(
                        self.ivars().smoke_preview_ticks.get() < 90,
                        "Framing window never became visible. Native presentation smoke requires an unlocked desktop and an unobscured Framing window. window={}, miniaturized={}, occlusion={:?}, visible_flag={:?}, owner={:?}, view={:?}, hidden={}, button={}, message={}",
                        c.framing.window.isVisible(),
                        c.framing.window.isMiniaturized(),
                        c.framing.window.occlusionState(),
                        NSWindowOcclusionState::Visible,
                        c.framing.preview.view.window().map(|w| (
                            w.windowNumber(),
                            w.isVisible(),
                            w.occlusionState()
                        )),
                        c.framing.preview.view.visibleRect(),
                        c.framing.preview.view.isHiddenOrHasHiddenAncestor(),
                        c.workspace.framing.isEnabled(),
                        self.ivars().state.borrow().message,
                    );
                    return;
                }
                self.smoke_check_presentation();
                self.smoke_begin_presentation_drag();
                // Run outside this timer invocation so the actual preview timer can fire.
                dispatch2::DispatchQueue::main().exec_async(|| {
                    let run_loop = objc2_foundation::NSRunLoop::mainRunLoop();
                    let until = objc2_foundation::NSDate::dateWithTimeIntervalSinceNow(0.25);
                    while until.timeIntervalSinceNow() > 0.0 {
                        run_loop.runMode_beforeDate(unsafe { NSEventTrackingRunLoopMode }, &until);
                    }
                });
            }
            10 => {
                assert!(self.ivars().smoke_preview_ticks.get() >= 3);
                c.framing.preview.smoke_check_gpu_completion();
                assert!(
                    self.ivars().smoke_tracking_ticks.get() >= 3,
                    "Preview timer stopped during event tracking"
                );
                self.smoke_check_presentation();
                let workspace = self.save_smoke_window(&c.window, "workspace-live.png");
                let framing = self.save_smoke_window(&c.framing.window, "framing.png");
                c.framing.preview.smoke_check_orientation(&framing);
                let id = self.ivars().smoke_preview.borrow().as_ref().unwrap().0;
                c.workspace.smoke_check_orientation(id, &workspace);
                self.smoke_end_presentation_drag();
                c.framing.preview.smoke_show_handles(false);
            }
            11 => {
                let clean = self.save_smoke_window(&c.framing.window, "framing-clean.png");
                c.framing.preview.smoke_check_orientation(&clean);
                c.framing.preview.smoke_show_handles(true);
                self.smoke_check_canvas_change();
                self.ivars().smoke_preview.borrow_mut().take();
                c.framing.window.performClose(None);
            }
            12 => {
                // No capture targets are enabled in normal UI smoke. Exercise
                // actual backend preflight rejection, never simulated encoding.
                let result = self
                    .ivars()
                    .capture
                    .borrow()
                    .group_command(crate::model::GroupCommand::Start, Vec::new());
                if let Err(error) = result {
                    assert!(!error.is_empty());
                    self.show_error(&error);
                    self.ivars().smoke_started.set(true);
                }
                self.ivars().smoke_preview_ticks.set(0);
            }
            13 => {
                let state = self.ivars().state.borrow();
                if state.group.state != crate::model::GroupState::Failed {
                    let ticks = self.ivars().smoke_preview_ticks.get() + 1;
                    self.ivars().smoke_preview_ticks.set(ticks);
                    assert!(
                        ticks < 100,
                        "Backend did not reject empty recording group within 20 seconds"
                    );
                    return;
                }
                assert!(
                    state
                        .group
                        .error
                        .as_ref()
                        .is_some_and(|error| !error.is_empty())
                );
                assert!(!state.configuration_locked());
                assert!(c.workspace.add.isEnabled());
                assert!(
                    c.workspace
                        .group_buttons
                        .iter()
                        .all(|button| !button.isEnabled())
                );
                self.ivars().smoke_started.set(true);
            }
            _ => unreachable!(),
        }
        self.ivars()
            .smoke_phase
            .set(self.ivars().smoke_phase.get() + 1);
    }

    fn smoke_click(&self, action: objc2::runtime::Sel) {
        // Exercise the same Objective-C target/action dispatch used by native controls.
        unsafe {
            let button = NSButton::buttonWithTitle_target_action(
                &ns("Smoke action"),
                Some(self),
                Some(action),
                self.mtm(),
            );
            button.performClick(None);
        }
    }

    fn save_smoke_image(&self, filename: &str) {
        let root = self.controls().window.contentView().unwrap();
        root.displayIfNeeded();
        let image = root
            .bitmapImageRepForCachingDisplayInRect(root.bounds())
            .expect("workspace bitmap");
        root.cacheDisplayInRect_toBitmapImageRep(root.bounds(), &image);
        // The empty property dictionary requests PNG defaults; all objects remain main-thread-local.
        let png = unsafe {
            image.representationUsingType_properties(
                NSBitmapImageFileType::PNG,
                &objc2_foundation::NSDictionary::new(),
            )
        }
        .expect("encode workspace PNG");
        let path = self
            .ivars()
            .smoke_directory
            .as_ref()
            .unwrap()
            .join(filename);
        assert!(png.writeToFile_atomically(&ns(&path.to_string_lossy()), true));
    }

    fn save_smoke_window(&self, window: &NSWindow, filename: &str) -> std::path::PathBuf {
        // AppKit's bitmap caching does not include CAMetalLayer drawables. Capture
        // this application's composited window instead, including sibling handles.
        let path = self
            .ivars()
            .smoke_directory
            .as_ref()
            .unwrap()
            .join(filename);
        let status = std::process::Command::new("/usr/sbin/screencapture")
            .args(["-x", "-o", &format!("-l{}", window.windowNumber())])
            .arg(&path)
            .status()
            .expect("capture composited smoke window");
        assert!(
            status.success(),
            "Cannot capture {} (check screen recording permission)",
            path.display()
        );
        assert!(path.metadata().unwrap().len() > 0);
        path
    }

    fn open_source(&self, draft: RecordingConfig, is_new: bool) {
        if !self.ivars().state.borrow().configuration_editable()
            || self.ivars().source_draft.borrow().is_some()
        {
            return;
        }
        let c = self.controls();
        c.name.setStringValue(&ns(&draft.name));
        *self.ivars().source_draft.borrow_mut() = Some(draft);
        self.ivars().source_is_new.set(is_new);
        self.ivars().video_reselected.set(false);
        c.source.error.setStringValue(&ns(""));
        c.source
            .save
            .setTitle(&ns(if is_new { "Add Source" } else { "Save Changes" }));
        c.source
            .window
            .setTitle(&ns(if is_new { "Add Source" } else { "Edit Source" }));
        self.refresh_pickers();
        c.window
            .beginSheet_completionHandler(&c.source.window, None);
        c.source.window.makeFirstResponder(Some(&*c.name));
    }

    fn read_source_selection(&self) {
        let c = self.controls();
        if let Some(draft) = self.ivars().source_draft.borrow_mut().as_mut() {
            if let Some(video) = self
                .ivars()
                .videos
                .borrow()
                .get(c.video.indexOfSelectedItem() as usize)
            {
                draft.video = video.clone();
            }
            if let Some(audio) = self
                .ivars()
                .audios
                .borrow()
                .get(c.audio.indexOfSelectedItem() as usize)
            {
                draft.audio = audio.clone();
            }
        }
    }

    fn commit_source(&self) {
        self.read_source_selection();
        let Some(mut draft) = self.ivars().source_draft.borrow().clone() else {
            return;
        };
        draft.name = self.controls().name.stringValue().to_string();
        let result = self.ivars().state.borrow_mut().apply_source_draft(
            draft,
            self.ivars().source_is_new.get(),
            self.ivars().video_reselected.get(),
        );
        if let Err(error) = result {
            self.controls().source.error.setStringValue(&ns(&error));
            return;
        }
        self.dismiss_source();
        self.refresh_editor();
        self.save();
    }

    fn dismiss_source(&self) {
        self.ivars().source_draft.borrow_mut().take();
        self.controls()
            .window
            .endSheet(&self.controls().source.window);
        self.controls().source.window.orderOut(None);
    }

    fn confirm_delete(&self) {
        if !self.ivars().state.borrow().configuration_editable()
            || self.ivars().delete_alert.borrow().is_some()
        {
            return;
        }
        let Some(recording) = self.ivars().state.borrow().selected().cloned() else {
            return;
        };
        let alert = NSAlert::new(self.mtm());
        alert.setMessageText(&ns(&format!("Delete ‘{}’?", recording.name)));
        alert.setInformativeText(&ns("This removes the configured video/audio pair. Files already saved on disk are not deleted."));
        alert.setAlertStyle(NSAlertStyle::Warning);
        // Return defaults to the safe option; deletion requires an explicit choice.
        alert.addButtonWithTitle(&ns("Cancel"));
        alert.addButtonWithTitle(&ns("Delete Source"));
        // The sheet and completion run on AppKit's main thread; retain only for its lifetime.
        let this =
            unsafe { Retained::retain(self as *const Self as *mut Self) }.expect("live delegate");
        let completion = block2::RcBlock::new(move |response: NSModalResponse| {
            this.ivars().delete_alert.borrow_mut().take();
            if response == NSAlertSecondButtonReturn {
                this.ivars()
                    .state
                    .borrow_mut()
                    .remove_confirmed(recording.id);
                this.refresh_editor();
                this.save();
            }
        });
        *self.ivars().delete_alert.borrow_mut() = Some(alert.clone());
        alert
            .beginSheetModalForWindow_completionHandler(&self.controls().window, Some(&completion));
    }

    fn save(&self) {
        self.ivars()
            .saver
            .borrow_mut()
            .save(&mut self.ivars().state.borrow_mut());
        self.refresh_status();
    }

    fn begin_refresh(&self) {
        if self.ivars().discovery.borrow().is_none() {
            *self.ivars().discovery.borrow_mut() = Some(discovery::refresh());
            self.ivars().last_refresh.set(Instant::now());
            self.ivars().device_refresh_due.set(false);
        }
    }

    fn commit_editor(&self, _video_reselected: bool) -> bool {
        // Selection and framing remain available while source/output fields are locked.
        if !self.ivars().state.borrow().configuration_editable() {
            return true;
        }
        let Some(c) = self.ivars().controls.get() else {
            return true;
        };
        let Some(mut recording) = self.ivars().state.borrow().selected().cloned() else {
            return true;
        };
        recording.output.directory = c.directory.stringValue().to_string().into();
        recording.output.filename_template = c.template.stringValue().to_string();
        let parsed = (
            c.width.stringValue().to_string().parse(),
            c.height.stringValue().to_string().parse(),
            c.fps.stringValue().to_string().parse(),
        );
        match parsed {
            (Ok(width), Ok(height), Ok(fps)) => {
                recording.output.resolution.width = width;
                recording.output.resolution.height = height;
                recording.output.fps = fps;
            }
            _ => {
                self.show_error("Width, height and FPS must be positive whole numbers.");
                return false;
            }
        }
        if recording.name.trim().is_empty() {
            self.show_error("Give this recording a name.");
            return false;
        }
        if !recording.output.directory.is_absolute() {
            self.show_error("Choose an output directory or enter its full absolute path.");
            return false;
        }
        if let Err(error) = recording.output.validate_format() {
            self.show_error(&error);
            return false;
        }
        let result = self
            .ivars()
            .state
            .borrow_mut()
            .apply_output(recording.id, recording.output);
        if let Err(error) = result {
            self.show_error(&error);
            return false;
        }
        true
    }

    fn show_error(&self, error: &str) {
        self.ivars().state.borrow_mut().message = error.into();
        self.controls().window.makeKeyAndOrderFront(None);
        self.refresh_status();
    }

    fn refresh_editor(&self) {
        let c = self.controls();
        let state = self.ivars().state.borrow();
        let selected = state.selected();
        let enabled = selected.is_some() && state.configuration_editable();
        for field in [&c.directory, &c.template, &c.width, &c.height, &c.fps] {
            field.setEnabled(enabled);
        }
        c.workspace.add.setEnabled(state.configuration_editable());
        for button in &c.edit_buttons {
            button.setEnabled(enabled);
        }
        c.workspace
            .framing
            .setEnabled(selected.is_some() && state.can_save);
        if let Some(recording) = selected {
            c.directory
                .setStringValue(&ns(&recording.output.directory.to_string_lossy()));
            c.template
                .setStringValue(&ns(&recording.output.filename_template));
            c.width
                .setStringValue(&ns(&recording.output.resolution.width.to_string()));
            c.height
                .setStringValue(&ns(&recording.output.resolution.height.to_string()));
            c.fps.setStringValue(&ns(&recording.output.fps.to_string()));
        } else {
            for field in [&c.directory, &c.template, &c.width, &c.height, &c.fps] {
                field.setStringValue(&ns(""));
            }
        }
        drop(state);
        self.refresh_pickers();
        self.refresh_list();
        self.refresh_status();
    }

    fn refresh_pickers(&self) {
        let state = self.ivars().state.borrow();
        let draft = self.ivars().source_draft.borrow();
        let Some(draft) = draft.as_ref() else {
            return;
        };
        let video = draft.video.clone();
        let audio = draft.audio.clone();
        let videos = video_options(&state.catalog.videos, &video);
        let audios = audio_options(&state.catalog.audios, &audio);
        let c = self.controls();
        c.video.removeAllItems();
        c.audio.removeAllItems();
        for (index, source) in videos.iter().enumerate() {
            let unavailable = !matches!(source, VideoSourceSelector::None)
                && !state.catalog.videos.iter().any(|s| same_video(s, source));
            let title = format!(
                "{}{}",
                video_title(source),
                if unavailable { " — Unavailable" } else { "" }
            );
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(self.mtm()),
                    &ns(&title),
                    None,
                    &ns(""),
                )
            };
            item.setTag(index as isize);
            c.video.menu().expect("popup menu").addItem(&item);
        }
        for (index, source) in audios.iter().enumerate() {
            let unavailable = !matches!(source, AudioSourceSelector::None)
                && !state.catalog.audios.iter().any(|s| same_audio(s, source));
            let title = format!(
                "{}{}",
                audio_title(source),
                if unavailable { " — Unavailable" } else { "" }
            );
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(self.mtm()),
                    &ns(&title),
                    None,
                    &ns(""),
                )
            };
            item.setTag(index as isize);
            c.audio.menu().expect("popup menu").addItem(&item);
        }
        c.video.selectItemAtIndex(
            videos
                .iter()
                .position(|s| same_video(s, &video))
                .unwrap_or(0) as isize,
        );
        c.audio.selectItemAtIndex(
            audios
                .iter()
                .position(|s| same_audio(s, &audio))
                .unwrap_or(0) as isize,
        );
        *self.ivars().videos.borrow_mut() = videos;
        *self.ivars().audios.borrow_mut() = audios;
    }

    fn refresh_list(&self) {
        workspace::refresh(self);
    }

    fn poll_capture(&self) {
        let mut worker = self.ivars().capture.borrow_mut();
        // A failed settings save can cancel termination after capture has stopped.
        if !self.ivars().quitting.get() && worker.is_stopping() && worker.is_finished() {
            *worker = CaptureWorker::new();
        }
        if !self.ivars().quitting.get() {
            let targets = if self.ivars().smoke_directory.is_some() {
                Vec::new()
            } else {
                self.ivars().state.borrow().capture_targets()
            };
            worker.update(targets);
        }
        if let Some(snapshot) = worker.poll() {
            self.ivars().state.borrow_mut().capture = snapshot;
            workspace::refresh_capture(self);
        }
    }

    fn refresh_status(&self) {
        let Some(c) = self.ivars().controls.get() else {
            return;
        };
        let state = self.ivars().state.borrow();
        let status = match state.selected() {
            Some(r) if state.needs_window_reselection(r.id) => {
                "Reselect this window: saved window IDs are not stable across app launches.".into()
            }
            Some(r) => state.recording_summary(r.id),
            None => "Add a recording to select its video, audio and destination.".into(),
        };
        c.status.setStringValue(&ns(&status));
        let mut guidance = permissions::guidance();
        if !state.catalog.warnings.is_empty() {
            guidance.push_str(&format!("\n{}", state.catalog.warnings.join("\n")));
        }
        if !state.capture.shutdown_errors.is_empty() {
            guidance.push_str(&format!("\n{}", state.capture.shutdown_errors.join("\n")));
        }
        if let Some(error) = &state.group.error {
            guidance.push_str(&format!("\nRecording: {error}"));
        }
        c.guidance.setStringValue(&ns(&guidance));
        c.message
            .setStringValue(&ns(if self.ivars().quit_after_group.get() {
                "Waiting for Stop & Save to finish before quitting…"
            } else if self.ivars().quitting.get() {
                "Stopping capture and saving settings…"
            } else {
                &state.message
            }));
        drop(state);
        self.refresh_group_controls();
    }

    fn button(
        &self,
        parent: &NSView,
        title: &str,
        frame: NSRect,
        action: objc2::runtime::Sel,
    ) -> Retained<NSButton> {
        let button = unsafe {
            NSButton::buttonWithTitle_target_action(
                &ns(title),
                Some(self),
                Some(action),
                self.mtm(),
            )
        };
        button.setFrame(frame);
        button.setBezelStyle(NSBezelStyle::Push);
        parent.addSubview(&button);
        button
    }

    fn build_window(&self) {
        let workspace = workspace::build(self);
        let source = dialogs::source(self);
        let global = dialogs::global(self);
        let framing = viewport::Framing::new(self);
        let _ = self.ivars().controls.set(Controls {
            window: workspace.window.clone(),
            name: source.name.clone(),
            video: source.video.clone(),
            audio: source.audio.clone(),
            directory: workspace.directory.clone(),
            template: workspace.template.clone(),
            width: workspace.width.clone(),
            height: workspace.height.clone(),
            fps: workspace.fps.clone(),
            status: workspace.status.clone(),
            message: workspace.message.clone(),
            guidance: global.guidance.clone(),
            edit_buttons: workspace.edit_buttons.clone(),
            workspace,
            source,
            global,
            framing,
        });
        workspace::layout(self);
        self.controls().window.center();
        self.controls().window.makeKeyAndOrderFront(None);
        #[allow(deprecated)]
        NSApplication::sharedApplication(self.mtm()).activateIgnoringOtherApps(true);
    }
}

fn ns(value: &str) -> Retained<NSString> {
    NSString::from_str(value)
}
fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}
fn label(
    parent: &NSView,
    text: &str,
    frame: NSRect,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    let label = NSTextField::wrappingLabelWithString(&ns(text), mtm);
    label.setFrame(frame);
    parent.addSubview(&label);
    label
}

pub fn run(smoke_directory: Option<std::path::PathBuf>) {
    let mtm = MainThreadMarker::new().expect("AppKit must launch on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let delegate = AppDelegate::new(mtm, smoke_directory);
    let menu = NSMenu::new(mtm);
    let item = NSMenuItem::new(mtm);
    let app_menu = NSMenu::new(mtm);
    let settings = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &ns("Settings…"),
            Some(sel!(showSettings:)),
            &ns(","),
        )
    };
    unsafe {
        settings.setTarget(Some(&*delegate));
    }
    app_menu.addItem(&settings);
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &ns("Quit Oculi"),
            Some(sel!(terminate:)),
            &ns("q"),
        )
    };
    app_menu.addItem(&quit);
    item.setSubmenu(Some(&app_menu));
    menu.addItem(&item);
    // Standard Edit actions keep keyboard cut/copy/paste available in text fields.
    let edit_item = NSMenuItem::new(mtm);
    edit_item.setTitle(&ns("Edit"));
    let edit = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns("Edit"));
    for (title, selector, key) in [
        ("Cut", sel!(cut:), "x"),
        ("Copy", sel!(copy:), "c"),
        ("Paste", sel!(paste:), "v"),
        ("Select All", sel!(selectAll:), "a"),
    ] {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &ns(title),
                Some(selector),
                &ns(key),
            )
        };
        edit.addItem(&item);
    }
    edit_item.setSubmenu(Some(&edit));
    menu.addItem(&edit_item);
    group::build_menu(&delegate, &menu);
    app.setMainMenu(Some(&menu));
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
}
