//! Resolution menu identities and drafts, independent of the other output fields.

use objc2::DefinedClass;
use objc2_app_kit::{NSEventTrackingRunLoopMode, NSTextField};

use super::{AppDelegate, ns};
use crate::app::AppState;
use crate::discovery::SourceCatalog;
use crate::model::Resolution;

#[derive(Default)]
pub(super) struct ResolutionDraft {
    baseline: Option<Resolution>,
    pub custom: bool,
    pub explicit: bool,
}

impl ResolutionDraft {
    fn dirty(&self, width: &str, height: &str) -> bool {
        self.explicit
            || self.baseline.is_some_and(|size| {
                width != size.width.to_string() || height != size.height.to_string()
            })
    }
}

fn title(size: Resolution, first: Option<Resolution>) -> String {
    let dimensions = format!("{} × {}", size.width, size.height);
    if first == Some(size) {
        return format!("First source · {dimensions}");
    }
    let name = match (size.width, size.height) {
        (640, 480) => "480p",
        (1280, 720) => "720p HD",
        (1920, 1080) => "1080p Full HD",
        (2560, 1440) => "1440p QHD",
        (3840, 2160) => "2160p 4K UHD",
        _ => return dimensions,
    };
    format!("{name} · {dimensions}")
}

pub(super) fn field_text(field: &NSTextField) -> String {
    field.currentEditor().map_or_else(
        || field.stringValue().to_string(),
        |editor| editor.string().to_string(),
    )
}

fn protect_pending_default(state: &mut AppState, draft_dirty: bool) -> Result<(), String> {
    if draft_dirty
        && state.configuration_editable()
        && let Some(mut recording) = state.selected().cloned()
        && recording.output.resolution_default_pending
    {
        recording.output.resolution_default_pending = false;
        state.apply_output(recording.id, recording.output)?;
    }
    Ok(())
}

fn menu_choices(
    mut sizes: Vec<Resolution>,
    draft_size: Option<Resolution>,
) -> Vec<Option<Resolution>> {
    if let Some(size) = draft_size {
        sizes.push(size);
    }
    crate::discovery::sort_resolutions(&mut sizes);
    sizes.into_iter().map(Some).chain([None]).collect()
}

impl AppDelegate {
    pub(super) fn resolution_draft_dirty(&self) -> bool {
        let c = self.controls();
        self.ivars()
            .resolution_draft
            .borrow()
            .dirty(&field_text(&c.width), &field_text(&c.height))
    }

    pub(super) fn reset_resolution_draft(&self) {
        let baseline = self
            .ivars()
            .state
            .borrow()
            .selected()
            .map(|r| r.output.resolution);
        *self.ivars().resolution_draft.borrow_mut() = ResolutionDraft {
            baseline,
            ..Default::default()
        };
        self.refresh_resolution_picker();
    }

    pub(super) fn resolution_draft_committed(&self) {
        let baseline = self
            .ivars()
            .state
            .borrow()
            .selected()
            .map(|r| r.output.resolution);
        let mut draft = self.ivars().resolution_draft.borrow_mut();
        draft.baseline = baseline;
        draft.explicit = false;
        drop(draft);
        self.refresh_resolution_picker();
    }

    pub(super) fn choose_resolution(&self) {
        if !self.ivars().state.borrow().configuration_editable() || self.ivars().quitting.get() {
            return;
        }
        let c = self.controls();
        let index = c.workspace.resolution.indexOfSelectedItem();
        let Some(size) = usize::try_from(index)
            .ok()
            .and_then(|i| self.ivars().resolutions.borrow().get(i).copied())
        else {
            return;
        };
        c.window.makeFirstResponder(None);
        let mut draft = self.ivars().resolution_draft.borrow_mut();
        draft.explicit = true;
        draft.custom = size.is_none();
        if let Some(size) = size {
            c.width.setStringValue(&ns(&size.width.to_string()));
            c.height.setStringValue(&ns(&size.height.to_string()));
        }
        drop(draft);
        self.refresh_group_controls();
    }

    pub(super) fn refresh_resolution_controls(&self) {
        let state = self.ivars().state.borrow();
        let enabled = state.configuration_editable()
            && state.selected().is_some()
            && !self.ivars().quitting.get();
        let c = self.controls();
        c.workspace.resolution.setEnabled(enabled);
        let custom = self.ivars().resolution_draft.borrow().custom;
        c.width.setEnabled(enabled && custom);
        c.height.setEnabled(enabled && custom);
    }

    pub(super) fn refresh_resolution_picker(&self) {
        if objc2_foundation::NSRunLoop::mainRunLoop()
            .currentMode()
            .as_deref()
            == Some(unsafe { NSEventTrackingRunLoopMode })
        {
            self.ivars().resolution_refresh_due.set(true);
            return;
        }
        self.ivars().resolution_refresh_due.set(false);
        let state = self.ivars().state.borrow();
        let first = state.first_source_resolution();
        let c = self.controls();
        let draft = self.ivars().resolution_draft.borrow();
        let selected = if draft.custom {
            None
        } else {
            field_text(&c.width)
                .parse()
                .ok()
                .zip(field_text(&c.height).parse().ok())
                .map(|(width, height)| Resolution { width, height })
        };
        let choices = menu_choices(state.resolution_choices(), selected);
        let labels: Vec<_> = choices
            .iter()
            .map(|size| size.map_or_else(|| "Custom…".into(), |size| title(size, first)))
            .collect();
        let popup = &c.workspace.resolution;
        if *self.ivars().resolution_labels.borrow() != labels {
            popup.removeAllItems();
            for label in &labels {
                popup.addItemWithTitle(&ns(label));
            }
            *self.ivars().resolution_labels.borrow_mut() = labels;
            *self.ivars().resolutions.borrow_mut() = choices.clone();
        }
        let index = choices
            .iter()
            .position(|size| *size == selected)
            .unwrap_or(choices.len() - 1);
        popup.selectItemAtIndex(index as isize);
        let hint = if state
            .selected()
            .is_some_and(|r| r.output.resolution_default_pending)
        {
            "Awaiting first source size · 1080p fallback. Choose a preset or Custom to override."
        } else {
            "Output canvas size · Custom enables width / height. Apply Output saves changes."
        };
        c.workspace.resolution_hint.setStringValue(&ns(hint));
        c.workspace.resolution_hint.setToolTip(Some(&ns(hint)));
        drop(draft);
        drop(state);
        self.refresh_resolution_controls();
    }

    pub(super) fn update_resolution_catalog(&self, catalog: SourceCatalog) {
        let draft_dirty = self.resolution_draft_dirty();
        let before = self
            .ivars()
            .state
            .borrow()
            .selected()
            .map(|r| (r.output.resolution, r.output.resolution_default_pending));
        // Protect even invalid in-progress text. Only the saved pending flag changes;
        // draft dimensions and all other output edits still wait for Apply / Return.
        let result = protect_pending_default(&mut self.ivars().state.borrow_mut(), draft_dirty);
        if let Err(error) = result {
            self.show_error(&error);
        }
        let changed = self.ivars().state.borrow().catalog != catalog;
        // This bool means ANY pending recording default resolved, not just selection.
        let resolved = self.ivars().state.borrow_mut().update_catalog(catalog);
        let after = self
            .ivars()
            .state
            .borrow()
            .selected()
            .map(|r| (r.output.resolution, r.output.resolution_default_pending));
        if !draft_dirty
            && before != after
            && let Some((size, _)) = after
        {
            let c = self.controls();
            c.width.setStringValue(&ns(&size.width.to_string()));
            c.height.setStringValue(&ns(&size.height.to_string()));
            self.ivars().resolution_draft.borrow_mut().baseline = Some(size);
        }
        if changed || resolved || before != after {
            self.refresh_pickers();
            self.refresh_list();
            self.refresh_resolution_picker();
        }
    }

    pub(super) fn smoke_select_resolution(&self, size: Option<Resolution>) {
        let index = self
            .ivars()
            .resolutions
            .borrow()
            .iter()
            .position(|choice| *choice == size)
            .expect("resolution menu choice");
        self.controls()
            .workspace
            .resolution
            .selectItemAtIndex(index as isize);
        assert!(unsafe {
            self.controls()
                .workspace
                .resolution
                .sendAction_to(Some(objc2::sel!(resolutionChanged:)), Some(self))
        });
    }

    pub(super) fn smoke_resolution_choices(&self) {
        let c = self.controls();
        assert!(c.workspace.resolution.isEnabled());
        assert!(!c.width.isEnabled());
        assert!(!c.height.isEnabled());
        assert!(self.commit_editor(false));
        assert!(
            self.ivars()
                .state
                .borrow()
                .selected()
                .unwrap()
                .output
                .resolution_default_pending
        );
        self.smoke_select_resolution(None);
        assert!(c.width.isEnabled() && c.height.isEnabled());
        assert!(self.commit_editor(false));
        assert!(
            !self
                .ivars()
                .state
                .borrow()
                .selected()
                .unwrap()
                .output
                .resolution_default_pending
        );
        let before = self
            .ivars()
            .state
            .borrow()
            .selected()
            .unwrap()
            .output
            .clone();
        c.template.setStringValue(&ns("{name}-resolution-smoke"));
        c.fps.setStringValue(&ns("60"));
        let size = Resolution {
            width: 1280,
            height: 720,
        };
        self.smoke_select_resolution(Some(size));
        assert_eq!(c.width.stringValue().to_string(), "1280");
        assert_eq!(c.height.stringValue().to_string(), "720");
        assert!(!c.width.isEnabled() && !c.height.isEnabled());
        assert_eq!(
            c.template.stringValue().to_string(),
            "{name}-resolution-smoke"
        );
        assert_eq!(c.fps.stringValue().to_string(), "60");
        assert_eq!(
            self.ivars().state.borrow().selected().unwrap().output,
            before
        );
        self.smoke_click(objc2::sel!(applyChanges:));
        assert_eq!(
            self.ivars()
                .state
                .borrow()
                .selected()
                .unwrap()
                .output
                .resolution,
            size
        );
    }

    pub(super) fn smoke_resolution_lock(&self) {
        use crate::model::GroupState;
        let snapshot = self.ivars().state.borrow().group.clone();
        let before = self
            .ivars()
            .state
            .borrow()
            .selected()
            .unwrap()
            .output
            .clone();
        for state in [
            GroupState::Preparing,
            GroupState::Recording,
            GroupState::Paused,
            GroupState::Finalizing,
        ] {
            self.ivars().state.borrow_mut().group.state = state;
            self.refresh_group_controls();
            let c = self.controls();
            assert!(!c.workspace.resolution.isEnabled());
            assert!(!c.width.isEnabled() && !c.height.isEnabled());
            self.smoke_select_resolution(Some(Resolution {
                width: 640,
                height: 480,
            }));
            assert!(self.commit_editor(false));
            assert_eq!(
                self.ivars().state.borrow().selected().unwrap().output,
                before
            );
        }
        self.ivars().state.borrow_mut().group = snapshot;
        self.refresh_resolution_picker();
        self.refresh_group_controls();
        assert!(self.controls().workspace.resolution.isEnabled());
        assert!(self.controls().width.isEnabled());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_fields_preserve_pending_but_custom_is_explicit() {
        let mut draft = ResolutionDraft {
            baseline: Some(Resolution::default()),
            ..Default::default()
        };
        assert!(!draft.dirty("1920", "1080"));
        draft.custom = true;
        draft.explicit = true;
        assert!(draft.dirty("1920", "1080"));
    }

    #[test]
    fn invalid_and_partial_manual_edits_are_dirty() {
        let draft = ResolutionDraft {
            baseline: Some(Resolution::default()),
            custom: true,
            explicit: false,
        };
        for (width, height) in [
            ("", "1080"),
            ("1921", "1080"),
            ("1920", "abc"),
            ("1280", "720"),
        ] {
            assert!(draft.dirty(width, height));
        }
    }

    #[test]
    fn first_source_label_takes_priority_without_claiming_encoding_support() {
        let size = Resolution {
            width: 3840,
            height: 2160,
        };
        assert_eq!(title(size, Some(size)), "First source · 3840 × 2160");
        assert_eq!(
            title(Resolution::default(), None),
            "1080p Full HD · 1920 × 1080"
        );
        assert_eq!(
            title(
                Resolution {
                    width: 1000,
                    height: 560
                },
                None
            ),
            "1000 × 560"
        );
    }

    #[test]
    fn menu_keeps_unsaved_native_choice_after_discovery_removes_it() {
        let native = Resolution {
            width: 3000,
            height: 2000,
        };
        let full_hd = Resolution::default();
        assert_eq!(
            menu_choices(vec![full_hd, full_hd], Some(native)),
            vec![Some(native), Some(full_hd), None]
        );
        assert_eq!(menu_choices(vec![], None), vec![None]);
    }

    #[test]
    fn late_discovery_protects_dirty_selection_but_resolves_other_pending_outputs() {
        use crate::model::{DeviceId, VideoSourceSelector};
        for (width, height, explicit) in [
            ("", "1080", false),
            ("1921", "1080", false),
            ("1280", "720", false),
            ("1920", "1080", true),
        ] {
            let mut state = AppState::from_settings(Default::default());
            state.add();
            state.add();
            let source = VideoSourceSelector::Camera {
                device_id: DeviceId("resolution-draft-test".into()),
                label: "Synthetic source".into(),
            };
            state.settings.recordings[0].video = source.clone();
            let original = state.selected().unwrap().output.clone();
            let draft = ResolutionDraft {
                baseline: Some(original.resolution),
                custom: true,
                explicit,
            };
            protect_pending_default(&mut state, draft.dirty(width, height)).unwrap();
            let native = Resolution {
                width: 3840,
                height: 2160,
            };
            assert!(state.update_catalog(SourceCatalog {
                videos: vec![source.clone()],
                video_resolutions: vec![(source, vec![native])],
                ..Default::default()
            }));
            let selected = &state.selected().unwrap().output;
            assert_eq!(selected.resolution, original.resolution);
            assert!(!selected.resolution_default_pending);
            assert_eq!(selected.directory, original.directory);
            assert_eq!(selected.filename_template, original.filename_template);
            assert_eq!(selected.fps, original.fps);
            assert_eq!(state.settings.recordings[0].output.resolution, native);
        }
    }

    #[test]
    fn untouched_and_locked_drafts_do_not_clear_pending_defaults() {
        let mut state = AppState::from_settings(Default::default());
        state.add();
        let original = state.selected().unwrap().output.clone();
        protect_pending_default(&mut state, false).unwrap();
        assert_eq!(state.selected().unwrap().output, original);
        state.group.state = crate::model::GroupState::Preparing;
        protect_pending_default(&mut state, true).unwrap();
        assert_eq!(state.selected().unwrap().output, original);
    }
}
