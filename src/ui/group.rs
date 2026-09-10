//! Group actions and asynchronous, encoder-safe application termination.

use objc2::{DefinedClass, MainThreadOnly, sel};
use objc2_app_kit::*;

use super::{AppDelegate, ns, workspace};
use crate::app::AppState;
use crate::model::{GroupCommand, GroupState};

#[derive(Debug, PartialEq, Eq)]
struct GroupControls {
    enabled: [bool; 3],
    pause_title: &'static str,
}

impl GroupControls {
    fn new(state: &AppState, editor_open: bool, quitting: bool) -> Self {
        let group = state.effective_group_state();
        Self {
            enabled: [
                !quitting && !editor_open && state.start_block_reason().is_none(),
                !quitting && matches!(group, GroupState::Recording | GroupState::Paused),
                !quitting && matches!(group, GroupState::Recording | GroupState::Paused),
            ],
            pause_title: if group == GroupState::Paused {
                "Resume"
            } else {
                "Pause"
            },
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum QuitAction {
    Wait,
    Stop,
    ShutdownCapture,
    Cancel,
}

fn quit_action(state: GroupState, stop_sent: bool) -> QuitAction {
    match state {
        GroupState::Preparing | GroupState::Finalizing => QuitAction::Wait,
        GroupState::Recording | GroupState::Paused if !stop_sent => QuitAction::Stop,
        GroupState::Recording | GroupState::Paused => QuitAction::Wait,
        GroupState::Idle => QuitAction::ShutdownCapture,
        GroupState::Failed => QuitAction::Cancel,
    }
}

impl AppDelegate {
    pub(super) fn start_recordings(&self) {
        if self.ivars().quitting.get() {
            return;
        }
        if self.ivars().source_draft.borrow().is_some()
            || self.ivars().delete_alert.borrow().is_some()
        {
            self.show_error("Finish or cancel the open source dialog before starting recordings.");
            return;
        }
        if self.ivars().state.borrow().configuration_locked() {
            return;
        }
        self.flush_viewport_edit();
        self.controls().window.makeFirstResponder(None);
        if !self.commit_editor(false) {
            return;
        }
        let result = self.ivars().state.borrow_mut().begin_group_start();
        let configs = match result {
            Ok(configs) => configs,
            Err(error) => {
                self.show_error(&error);
                return;
            }
        };
        // Lock before enqueueing, then wait for this command's processed sequence.
        self.refresh_group_controls();
        let result = self
            .ivars()
            .capture
            .borrow()
            .group_command(GroupCommand::Start, configs);
        match result {
            Ok(sequence) => self.ivars().state.borrow_mut().accept_group_start(sequence),
            Err(error) => self.ivars().state.borrow_mut().reject_group_start(error),
        }
        self.save();
        self.refresh_status();
    }

    pub(super) fn pause_recordings(&self) {
        if self.ivars().quitting.get() {
            return;
        }
        let command = match self.ivars().state.borrow().effective_group_state() {
            GroupState::Recording => GroupCommand::Pause,
            GroupState::Paused => GroupCommand::Resume,
            _ => return,
        };
        self.send_group_command(command);
    }

    pub(super) fn stop_recordings(&self) {
        if self.ivars().quitting.get() {
            return;
        }
        if matches!(
            self.ivars().state.borrow().effective_group_state(),
            GroupState::Recording | GroupState::Paused
        ) {
            self.send_group_command(GroupCommand::Stop);
        }
    }

    fn send_group_command(&self, command: GroupCommand) -> bool {
        let result = self
            .ivars()
            .capture
            .borrow()
            .group_command(command, Vec::new());
        if let Err(error) = result {
            self.show_error(&error);
            return false;
        }
        true
    }

    pub(super) fn poll_group(&self) {
        let snapshot = self.ivars().capture.borrow().group_snapshot();
        self.ivars().state.borrow_mut().update_group(snapshot);
    }

    pub(super) fn refresh_group_controls(&self) {
        let c = self.controls();
        let state = self.ivars().state.borrow();
        let editor_open = self.ivars().source_draft.borrow().is_some()
            || self.ivars().delete_alert.borrow().is_some();
        let controls = GroupControls::new(&state, editor_open, self.ivars().quitting.get());
        c.workspace.group_buttons[1].setTitle(&ns(controls.pause_title));
        let reason = if editor_open {
            Some("Finish or cancel the open source dialog before starting.".into())
        } else {
            state.start_block_reason()
        };
        for (button, enabled) in c.workspace.group_buttons.iter().zip(controls.enabled) {
            button.setEnabled(enabled);
        }
        c.workspace.group_buttons[0].setToolTip(reason.as_deref().map(ns).as_deref());
        if let Some(items) = self.ivars().group_menu.get() {
            items[1].setTitle(&ns(controls.pause_title));
            for (item, enabled) in items.iter().zip(controls.enabled) {
                item.setEnabled(enabled);
            }
        }
        let editable = state.configuration_editable() && !self.ivars().quitting.get();
        c.workspace.add.setEnabled(editable);
        for button in &c.edit_buttons {
            button.setEnabled(editable && state.selected().is_some());
        }
        for field in [&c.directory, &c.template, &c.width, &c.height, &c.fps] {
            field.setEnabled(editable && state.selected().is_some());
        }
        c.workspace
            .framing
            .setEnabled(state.can_save && state.selected().is_some());
        c.name.setEnabled(editable);
        c.video.setEnabled(editable);
        c.audio.setEnabled(editable);
        c.source.save.setEnabled(editable);
        drop(state);
        workspace::refresh_group(self);
    }

    pub(super) fn advance_group_quit(&self) {
        if !self.ivars().quit_after_group.get() {
            return;
        }
        let state = self.ivars().state.borrow().effective_group_state();
        match quit_action(state, self.ivars().quit_stop_sent.get()) {
            QuitAction::Wait => {}
            QuitAction::Stop => {
                if self.send_group_command(GroupCommand::Stop) {
                    self.ivars().quit_stop_sent.set(true);
                } else {
                    self.cancel_group_quit();
                }
            }
            QuitAction::Cancel => {
                let error = self
                    .ivars()
                    .state
                    .borrow()
                    .group
                    .error
                    .clone()
                    .unwrap_or_else(|| {
                        "Recording failed. Review each output's error before quitting.".into()
                    });
                self.cancel_group_quit();
                self.show_error(&format!("Quit canceled: {error}"));
            }
            QuitAction::ShutdownCapture => {
                self.ivars().quit_after_group.set(false);
                self.ivars().capture.borrow().shutdown();
            }
        }
    }

    fn cancel_group_quit(&self) {
        self.ivars().quit_after_group.set(false);
        self.ivars().quit_stop_sent.set(false);
        self.ivars().quitting.set(false);
        NSApplication::sharedApplication(self.mtm()).replyToApplicationShouldTerminate(false);
    }
}

pub(super) fn build_menu(delegate: &AppDelegate, menu: &NSMenu) {
    let mtm = delegate.mtm();
    let root = NSMenuItem::new(mtm);
    root.setTitle(&ns("Recording"));
    let submenu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns("Recording"));
    submenu.setAutoenablesItems(false);
    let mut items = Vec::new();
    for (title, action) in [
        ("Start", sel!(startGroup:)),
        ("Pause", sel!(pauseGroup:)),
        ("Stop & Save", sel!(stopGroup:)),
    ] {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &ns(title),
                Some(action),
                &ns(""),
            )
        };
        unsafe {
            item.setTarget(Some(delegate));
        }
        item.setEnabled(false);
        submenu.addItem(&item);
        items.push(item);
    }
    root.setSubmenu(Some(&submenu));
    menu.addItem(&root);
    let _ = delegate.ivars().group_menu.set(items);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::coordinator::GroupSnapshot;
    use crate::storage::PersistedSettings;

    #[test]
    fn quit_never_shuts_down_capture_until_finalization_is_terminal() {
        assert_eq!(quit_action(GroupState::Preparing, false), QuitAction::Wait);
        assert_eq!(quit_action(GroupState::Recording, false), QuitAction::Stop);
        assert_eq!(quit_action(GroupState::Paused, false), QuitAction::Stop);
        assert_eq!(quit_action(GroupState::Recording, true), QuitAction::Wait);
        assert_eq!(quit_action(GroupState::Paused, true), QuitAction::Wait);
        for stop_sent in [false, true] {
            assert_eq!(
                quit_action(GroupState::Finalizing, stop_sent),
                QuitAction::Wait
            );
            assert_eq!(
                quit_action(GroupState::Failed, stop_sent),
                QuitAction::Cancel
            );
            assert_eq!(
                quit_action(GroupState::Idle, stop_sent),
                QuitAction::ShutdownCapture
            );
        }
    }

    #[test]
    fn group_controls_follow_snapshots_without_claiming_capture_success() {
        let mut state = AppState::from_settings(PersistedSettings::default());
        assert_eq!(GroupControls::new(&state, false, false).enabled, [false; 3]);
        for (group, enabled, title) in [
            (GroupState::Preparing, [false; 3], "Pause"),
            (GroupState::Recording, [false, true, true], "Pause"),
            (GroupState::Paused, [false, true, true], "Resume"),
            (GroupState::Finalizing, [false; 3], "Pause"),
            (GroupState::Failed, [false; 3], "Pause"),
        ] {
            state.update_group(GroupSnapshot {
                state: group,
                ..Default::default()
            });
            assert_eq!(
                GroupControls::new(&state, false, false),
                GroupControls {
                    enabled,
                    pause_title: title
                }
            );
            assert_eq!(GroupControls::new(&state, false, true).enabled, [false; 3]);
        }
    }
}
