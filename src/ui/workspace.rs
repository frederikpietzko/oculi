//! Native workspace with clean live output cards and capture diagnostics.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_foundation::{NSPoint, NSRect, NSSize};

use super::source_picker::{audio_title, video_title};
use super::viewport::Preview;
use super::{AppDelegate, label, ns, rect};
use crate::model::{RecordingConfig, RecordingId};
use crate::render::RenderedFrame;

const SIDEBAR: f64 = 240.0;
const FOOTER: f64 = 100.0;
const HEADER: f64 = 96.0;
const DRAWER_OPEN: f64 = 232.0;
const DRAWER_CLOSED: f64 = 44.0;
const GAP: f64 = 20.0;
const GRID_INSET: f64 = 8.0;
const ROW_HEIGHT: f64 = 68.0;

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    struct WorkspaceView;

    impl WorkspaceView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }
    }
);

pub(super) struct Workspace {
    pub window: Retained<NSWindow>,
    pub list: Retained<NSView>,
    pub grid: Retained<NSView>,
    pub grid_scroll: Retained<NSScrollView>,
    pub directory: Retained<NSTextField>,
    pub template: Retained<NSTextField>,
    pub width: Retained<NSTextField>,
    pub height: Retained<NSTextField>,
    pub fps: Retained<NSTextField>,
    pub status: Retained<NSTextField>,
    pub message: Retained<NSTextField>,
    pub edit_buttons: Vec<Retained<NSButton>>,
    pub add: Retained<NSButton>,
    pub edit: Retained<NSButton>,
    pub duplicate: Retained<NSButton>,
    pub remove: Retained<NSButton>,
    pub framing: Retained<NSButton>,
    pub drawer_toggle: Retained<NSButton>,
    pub group_buttons: Vec<Retained<NSButton>>,
    root: Retained<NSView>,
    sidebar: Retained<NSBox>,
    sidebar_title: Retained<NSTextField>,
    sidebar_hint: Retained<NSTextField>,
    list_scroll: Retained<NSScrollView>,
    heading: Retained<NSTextField>,
    count: Retained<NSTextField>,
    refresh: Retained<NSButton>,
    drawer: Retained<NSBox>,
    drawer_body: Retained<NSView>,
    output_name: Retained<NSTextField>,
    output_labels: Vec<Retained<NSTextField>>,
    output_hint: Retained<NSTextField>,
    choose: Retained<NSButton>,
    apply: Retained<NSButton>,
    footer: Retained<NSBox>,
    group_title: Retained<NSTextField>,
    group_reason: Retained<NSTextField>,
    empty_title: Retained<NSTextField>,
    empty_hint: Retained<NSTextField>,
    rows: RefCell<Vec<SourceRow>>,
    cards: RefCell<Vec<Card>>,
}

struct SourceRow {
    view: Retained<NSView>,
    background: Retained<NSBox>,
    title: Retained<NSTextField>,
    detail: Retained<NSTextField>,
    select: Retained<NSButton>,
}

struct Card {
    id: RecordingId,
    view: Retained<NSView>,
    background: Retained<NSBox>,
    preview: Preview,
    name: Retained<NSTextField>,
    video: Retained<NSTextField>,
    audio: Retained<NSTextField>,
    report: Retained<NSTextField>,
    selection: Retained<NSTextField>,
    select: Retained<NSButton>,
}

#[derive(Debug)]
struct GridGeometry {
    columns: usize,
    card_width: f64,
    card_height: f64,
    document_height: f64,
}

impl GridGeometry {
    fn new(width: f64, viewport_height: f64, count: usize) -> Self {
        let available = (width - 2.0 * GRID_INSET).max(1.0);
        let columns = ((available + GAP) / (340.0 + GAP)).floor().max(1.0) as usize;
        let card_width = (available - GAP * (columns - 1) as f64) / columns as f64;
        let card_height = (card_width - 24.0).max(0.0) * 9.0 / 16.0 + 174.0;
        let rows = count.div_ceil(columns);
        let content_height =
            2.0 * GRID_INSET + rows as f64 * card_height + rows.saturating_sub(1) as f64 * GAP;
        Self {
            columns,
            card_width,
            card_height,
            document_height: viewport_height.max(content_height),
        }
    }

    fn frame(&self, index: usize) -> NSRect {
        rect(
            GRID_INSET + (index % self.columns) as f64 * (self.card_width + GAP),
            self.document_height
                - GRID_INSET
                - self.card_height
                - (index / self.columns) as f64 * (self.card_height + GAP),
            self.card_width,
            self.card_height,
        )
    }
}

fn view(parent: &NSView, mtm: MainThreadMarker) -> Retained<NSView> {
    let view = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, 1.0, 1.0));
    parent.addSubview(&view);
    view
}

fn panel(parent: &NSView, color: &NSColor, mtm: MainThreadMarker) -> Retained<NSBox> {
    let panel = NSBox::initWithFrame(NSBox::alloc(mtm), rect(0.0, 0.0, 1.0, 1.0));
    panel.setBoxType(NSBoxType::Custom);
    panel.setTitlePosition(NSTitlePosition::NoTitle);
    panel.setFillColor(color);
    panel.setBorderColor(&NSColor::separatorColor());
    panel.setBorderWidth(0.0);
    panel.setContentViewMargins(NSSize::new(0.0, 0.0));
    parent.addSubview(&panel);
    panel
}

fn text(
    parent: &NSView,
    value: &str,
    size: f64,
    bold: bool,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    let field = label(parent, value, rect(0.0, 0.0, 1.0, 1.0), mtm);
    let font = if bold {
        NSFont::boldSystemFontOfSize(size)
    } else {
        NSFont::systemFontOfSize(size)
    };
    field.setFont(Some(&font));
    field.setTextColor(Some(&NSColor::labelColor()));
    field.setMaximumNumberOfLines(1);
    field.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    field.setToolTip(Some(&ns(value)));
    field
}

fn secondary(parent: &NSView, value: &str, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let field = text(parent, value, 12.0, false, mtm);
    field.setTextColor(Some(&NSColor::secondaryLabelColor()));
    field
}

fn scroll(parent: &NSView, mtm: MainThreadMarker) -> (Retained<NSScrollView>, Retained<NSView>) {
    let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), rect(0.0, 0.0, 1.0, 1.0));
    scroll.setHasVerticalScroller(true);
    scroll.setHasHorizontalScroller(false);
    scroll.setAutohidesScrollers(true);
    scroll.setDrawsBackground(false);
    let document = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, 1.0, 1.0));
    scroll.setDocumentView(Some(&document));
    parent.addSubview(&scroll);
    (scroll, document)
}

fn field(parent: &NSView, delegate: &AppDelegate) -> Retained<NSTextField> {
    let field = NSTextField::textFieldWithString(&ns(""), delegate.mtm());
    field.setFont(Some(&NSFont::systemFontOfSize(13.0)));
    unsafe {
        field.setTarget(Some(delegate));
        field.setAction(Some(sel!(applyChanges:)));
    }
    parent.addSubview(&field);
    field
}

fn selection_button(
    parent: &NSView,
    title: &str,
    index: usize,
    delegate: &AppDelegate,
) -> Retained<NSButton> {
    let button = delegate.button(parent, "", rect(0.0, 0.0, 1.0, 1.0), sel!(selectRecording:));
    button.setBordered(false);
    button.setTransparent(true);
    button.setTag(index as isize);
    button.setToolTip(Some(&ns(title)));
    button.setAccessibilityLabel(Some(&ns(title)));
    button
}

pub(super) fn build(delegate: &AppDelegate) -> Workspace {
    let mtm = delegate.mtm();
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect(0.0, 0.0, 1240.0, 880.0),
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Miniaturizable
                | NSWindowStyleMask::Resizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(&ns("Oculi"));
    window.setContentMinSize(NSSize::new(1080.0, 760.0));
    window.setBackgroundColor(Some(&NSColor::windowBackgroundColor()));
    window.setDelegate(Some(ProtocolObject::from_ref(delegate)));
    unsafe { window.setReleasedWhenClosed(false) };
    let root = WorkspaceView::alloc(mtm).set_ivars(());
    let root: Retained<WorkspaceView> =
        unsafe { msg_send![super(root), initWithFrame: rect(0.0, 0.0, 1240.0, 880.0)] };
    let root: Retained<NSView> = root.into_super();
    root.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    window.setContentView(Some(&root));
    // Do not auto-edit an output field on launch. That eagerly creates macOS's
    // text-input cursor window, which can briefly appear as an empty window.
    window.setInitialFirstResponder(Some(&root));

    let background = panel(&root, &NSColor::windowBackgroundColor(), mtm);
    background.setFrame(root.bounds());
    background.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    let sidebar = panel(&root, &NSColor::controlBackgroundColor(), mtm);
    let sidebar_title = text(&root, "Sources", 17.0, true, mtm);
    let sidebar_hint = secondary(&root, "One video + one audio per recording", mtm);
    let add = delegate.button(
        &root,
        "Add Source…",
        rect(0.0, 0.0, 1.0, 1.0),
        sel!(addRecording:),
    );
    let edit = delegate.button(
        &root,
        "Edit…",
        rect(0.0, 0.0, 1.0, 1.0),
        sel!(editRecording:),
    );
    let duplicate = delegate.button(
        &root,
        "Duplicate",
        rect(0.0, 0.0, 1.0, 1.0),
        sel!(duplicateRecording:),
    );
    let remove = delegate.button(
        &root,
        "Delete",
        rect(0.0, 0.0, 1.0, 1.0),
        sel!(removeRecording:),
    );
    let framing = delegate.button(
        &root,
        "Framing…",
        rect(0.0, 0.0, 1.0, 1.0),
        sel!(showFraming:),
    );
    let (list_scroll, list) = scroll(&root, mtm);
    let heading = text(&root, "Recording workspace", 25.0, true, mtm);
    let count = secondary(&root, "No recording pairs configured", mtm);
    let refresh = delegate.button(
        &root,
        "Refresh Sources",
        rect(0.0, 0.0, 1.0, 1.0),
        sel!(refreshSources:),
    );
    refresh.setBordered(false);
    let (grid_scroll, grid) = scroll(&root, mtm);
    let empty_title = text(
        &grid,
        "Your workspace starts with a source",
        20.0,
        true,
        mtm,
    );
    let empty_hint = secondary(
        &grid,
        "Use Add Source… to configure a video and audio pair with a live output preview.",
        mtm,
    );
    empty_title.setAlignment(NSTextAlignment::Center);
    empty_hint.setAlignment(NSTextAlignment::Center);
    empty_hint.setMaximumNumberOfLines(2);
    empty_hint.setLineBreakMode(NSLineBreakMode::ByWordWrapping);

    let drawer = panel(&root, &NSColor::controlBackgroundColor(), mtm);
    drawer.setBorderWidth(1.0);
    let drawer_toggle = delegate.button(
        &root,
        "▾  Recording output",
        rect(0.0, 0.0, 1.0, 1.0),
        sel!(toggleDrawer:),
    );
    drawer_toggle.setBordered(false);
    let output_name = secondary(&root, "Select a recording", mtm);
    let drawer_body = view(&root, mtm);
    let output_labels = ["Save to", "Filename template", "Width", "Height", "FPS"]
        .into_iter()
        .map(|value| secondary(&drawer_body, value, mtm))
        .collect();
    let directory = field(&drawer_body, delegate);
    let template = field(&drawer_body, delegate);
    let width = field(&drawer_body, delegate);
    let height = field(&drawer_body, delegate);
    let fps = field(&drawer_body, delegate);
    for (field, name) in [
        (&directory, "Output directory"),
        (&template, "Filename template"),
        (&width, "Output width"),
        (&height, "Output height"),
        (&fps, "Frames per second"),
    ] {
        field.setAccessibilityLabel(Some(&ns(name)));
    }
    let choose = delegate.button(
        &drawer_body,
        "Choose…",
        rect(0.0, 0.0, 1.0, 1.0),
        sel!(chooseDirectory:),
    );
    let apply = delegate.button(
        &drawer_body,
        "Apply Output",
        rect(0.0, 0.0, 1.0, 1.0),
        sel!(applyChanges:),
    );
    let output_hint = secondary(
        &drawer_body,
        "Tokens: {name}  {date}  {time}  {counter}    ·    MP4 / H.264 / AAC",
        mtm,
    );
    let status = secondary(&drawer_body, "", mtm);

    let footer = panel(&root, &NSColor::windowBackgroundColor(), mtm);
    footer.setBorderWidth(1.0);
    let group_title = text(&root, "Group recording", 13.0, true, mtm);
    let group_reason = secondary(
        &root,
        "Add available video and audio sources to start recording.",
        mtm,
    );
    let message = secondary(&root, "", mtm);
    let group_buttons = [
        ("Start", sel!(startGroup:)),
        ("Pause", sel!(pauseGroup:)),
        ("Stop & Save", sel!(stopGroup:)),
    ]
    .into_iter()
    .map(|(title, action)| {
        let button = delegate.button(&root, title, rect(0.0, 0.0, 1.0, 1.0), action);
        button.setEnabled(false);
        button
    })
    .collect();

    let workspace = Workspace {
        window,
        list,
        grid,
        grid_scroll,
        directory,
        template,
        width,
        height,
        fps,
        status,
        message,
        edit_buttons: vec![
            edit.clone(),
            duplicate.clone(),
            remove.clone(),
            choose.clone(),
            apply.clone(),
            framing.clone(),
        ],
        add,
        edit,
        duplicate,
        remove,
        framing,
        drawer_toggle,
        group_buttons,
        root,
        sidebar,
        sidebar_title,
        sidebar_hint,
        list_scroll,
        heading,
        count,
        refresh,
        drawer,
        drawer_body,
        output_name,
        output_labels,
        output_hint,
        choose,
        apply,
        footer,
        group_title,
        group_reason,
        empty_title,
        empty_hint,
        rows: RefCell::new(Vec::new()),
        cards: RefCell::new(Vec::new()),
    };
    workspace.set_frames(delegate.ivars().drawer_open.get());
    workspace.window.center();
    workspace
}

pub(super) fn refresh(delegate: &AppDelegate) {
    let workspace = &delegate.controls().workspace;
    let mtm = delegate.mtm();
    let state = delegate.ivars().state.borrow();
    let enabled = state.configuration_editable() && state.selected().is_some();
    workspace.add.setEnabled(state.configuration_editable());
    for button in &workspace.edit_buttons {
        button.setEnabled(enabled);
    }
    workspace
        .framing
        .setEnabled(state.can_save && state.selected().is_some());
    for field in [
        &workspace.directory,
        &workspace.template,
        &workspace.width,
        &workspace.height,
        &workspace.fps,
    ] {
        field.setEnabled(enabled);
    }
    for row in workspace.rows.borrow_mut().drain(..) {
        row.view.removeFromSuperview();
    }
    for card in workspace.cards.borrow_mut().drain(..) {
        card.view.removeFromSuperview();
    }
    let count = state.settings.recordings.len();
    workspace.count.setStringValue(&ns(&format!(
        "{count} recording {}",
        if count == 1 { "pair" } else { "pairs" }
    )));
    workspace.empty_title.setHidden(count != 0);
    workspace.empty_hint.setHidden(count != 0);
    workspace.output_name.setStringValue(&ns(state
        .selected()
        .map(|recording| recording.name.as_str())
        .unwrap_or("Select a recording")));
    workspace
        .output_name
        .setToolTip(Some(&workspace.output_name.stringValue()));
    for (index, recording) in state.settings.recordings.iter().enumerate() {
        let selected = Some(recording.id) == state.selected;
        let row_view = view(&workspace.list, mtm);
        let row_color = if selected {
            NSColor::selectedContentBackgroundColor()
        } else {
            NSColor::controlBackgroundColor()
        };
        let background = panel(&row_view, &row_color, mtm);
        background.setCornerRadius(8.0);
        let title = text(&row_view, &recording.name, 13.0, true, mtm);
        let detail = secondary(&row_view, &state.capture_summary(recording.id), mtm);
        if selected {
            title.setTextColor(Some(&NSColor::alternateSelectedControlTextColor()));
            detail.setTextColor(Some(&NSColor::alternateSelectedControlTextColor()));
        }
        let select = selection_button(
            &row_view,
            &format!(
                "Select {}{}",
                recording.name,
                if selected { ", selected" } else { "" }
            ),
            index,
            delegate,
        );
        workspace.rows.borrow_mut().push(SourceRow {
            view: row_view,
            background,
            title,
            detail,
            select,
        });

        let card_view = view(&workspace.grid, mtm);
        let background = panel(&card_view, &NSColor::controlBackgroundColor(), mtm);
        background.setCornerRadius(12.0);
        background.setBorderWidth(if selected { 2.0 } else { 1.0 });
        let border_color = if selected {
            NSColor::controlAccentColor()
        } else {
            NSColor::separatorColor()
        };
        background.setBorderColor(&border_color);
        let preview = Preview::new(mtm, false);
        card_view.addSubview(&preview.view);
        let name = text(&card_view, &recording.name, 15.0, true, mtm);
        let video = secondary(
            &card_view,
            &format!("Video · {}", video_title(&recording.video)),
            mtm,
        );
        let audio = secondary(
            &card_view,
            &format!("Audio · {}", audio_title(&recording.audio)),
            mtm,
        );
        let report = secondary(&card_view, &state.recording_summary(recording.id), mtm);
        report.setMaximumNumberOfLines(3);
        report.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        report.setSelectable(true);
        let selection = text(
            &card_view,
            if selected { "Selected" } else { "" },
            11.0,
            true,
            mtm,
        );
        selection.setTextColor(Some(&NSColor::controlAccentColor()));
        selection.setAlignment(NSTextAlignment::Right);
        let select = selection_button(
            &card_view,
            &format!(
                "Select {}. Video: {}. Audio: {}. Use Framing to edit the live output.{}",
                recording.name,
                video_title(&recording.video),
                audio_title(&recording.audio),
                if selected { " Selected." } else { "" }
            ),
            index,
            delegate,
        );
        workspace.cards.borrow_mut().push(Card {
            id: recording.id,
            view: card_view,
            background,
            preview,
            name,
            video,
            audio,
            report,
            selection,
            select,
        });
    }
    drop(state);
    layout(delegate);
}

pub(super) fn layout(delegate: &AppDelegate) {
    if let Some(controls) = delegate.ivars().controls.get() {
        controls
            .workspace
            .set_frames(delegate.ivars().drawer_open.get());
    }
}

pub(super) fn refresh_capture(delegate: &AppDelegate) {
    let workspace = &delegate.controls().workspace;
    let state = delegate.ivars().state.borrow();
    for (recording, row) in state
        .settings
        .recordings
        .iter()
        .zip(workspace.rows.borrow().iter())
    {
        let summary = ns(&state.recording_summary(recording.id));
        row.detail.setStringValue(&summary);
        row.detail.setToolTip(Some(&summary));
    }
}

pub(super) fn refresh_group(delegate: &AppDelegate) {
    let workspace = &delegate.controls().workspace;
    let state = delegate.ivars().state.borrow();
    workspace.group_title.setStringValue(&ns("Group recording"));
    let summary = state.group_summary();
    let reason = state.group.error.as_ref().cloned().unwrap_or_else(|| {
        if state.configuration_locked() {
            summary.clone()
        } else {
            state
                .start_block_reason()
                .unwrap_or_else(|| summary.clone())
        }
    });
    workspace.group_reason.setStringValue(&ns(&reason));
    workspace.group_reason.setToolTip(Some(&ns(&reason)));
    workspace.count.setStringValue(&ns(&format!(
        "{} recording pairs · {summary}",
        state.settings.recordings.len()
    )));
    for card in workspace.cards.borrow().iter() {
        let report = ns(&state.recording_summary(card.id));
        card.report.setStringValue(&report);
        card.report.setToolTip(Some(&report));
    }
    drop(state);
    refresh_capture(delegate);
}

impl Workspace {
    pub fn smoke_gpu_ready(&self, id: RecordingId) -> bool {
        self.cards
            .borrow()
            .iter()
            .any(|card| card.id == id && card.preview.smoke_gpu_ready())
    }

    pub fn smoke_check_orientation(&self, id: RecordingId, screenshot: &std::path::Path) {
        let cards = self.cards.borrow();
        let card = cards
            .iter()
            .find(|card| card.id == id)
            .expect("synthetic preview card");
        card.preview.smoke_check_orientation(screenshot);
    }

    pub fn smoke_check_presented(&self, id: RecordingId) {
        let cards = self.cards.borrow();
        let card = cards
            .iter()
            .find(|card| card.id == id)
            .expect("synthetic preview card");
        card.preview.smoke_check_presented();
    }

    pub fn smoke_check_layers(&self) {
        for card in self.cards.borrow().iter() {
            card.preview.smoke_check_layer();
        }
    }

    pub fn preview_visible(&self, id: RecordingId) -> bool {
        self.cards
            .borrow()
            .iter()
            .any(|card| card.id == id && card.preview.visible())
    }

    pub fn update_preview(
        &self,
        recording: &RecordingConfig,
        frame: Option<Result<RenderedFrame, String>>,
        status: &str,
    ) {
        if let Some(card) = self
            .cards
            .borrow()
            .iter()
            .find(|card| card.id == recording.id)
            && card.preview.visible()
        {
            card.preview.update(recording, frame, status, false);
        }
    }
    fn set_frames(&self, drawer_open: bool) {
        let size = self.root.bounds().size;
        let width = size.width;
        let height = size.height;
        let main_width = width - SIDEBAR;
        let drawer_height = if drawer_open {
            DRAWER_OPEN
        } else {
            DRAWER_CLOSED
        };
        self.sidebar
            .setFrame(rect(0.0, FOOTER, SIDEBAR, height - FOOTER));
        self.sidebar_title
            .setFrame(rect(20.0, height - 47.0, 200.0, 24.0));
        self.add.setFrame(rect(16.0, height - 96.0, 208.0, 32.0));
        self.sidebar_hint
            .setFrame(rect(20.0, FOOTER + 18.0, 204.0, 34.0));
        self.sidebar_hint.setMaximumNumberOfLines(2);
        self.sidebar_hint
            .setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        self.edit.setFrame(rect(12.0, FOOTER + 96.0, 106.0, 30.0));
        self.duplicate
            .setFrame(rect(120.0, FOOTER + 96.0, 108.0, 30.0));
        self.remove.setFrame(rect(12.0, FOOTER + 60.0, 106.0, 30.0));
        self.framing
            .setFrame(rect(120.0, FOOTER + 60.0, 108.0, 30.0));
        self.list_scroll.setFrame(rect(
            8.0,
            FOOTER + 140.0,
            SIDEBAR - 16.0,
            (height - FOOTER - 250.0).max(0.0),
        ));
        self.heading.setFrame(rect(
            SIDEBAR + 24.0,
            height - 48.0,
            main_width - 210.0,
            32.0,
        ));
        self.count.setFrame(rect(
            SIDEBAR + 24.0,
            height - 74.0,
            main_width - 210.0,
            20.0,
        ));
        self.refresh
            .setFrame(rect(width - 168.0, height - 54.0, 144.0, 32.0));
        self.grid_scroll.setFrame(rect(
            SIDEBAR + 16.0,
            FOOTER + drawer_height + 12.0,
            main_width - 32.0,
            (height - FOOTER - drawer_height - HEADER - 12.0).max(0.0),
        ));

        self.drawer
            .setFrame(rect(SIDEBAR, FOOTER, main_width, drawer_height));
        self.drawer_toggle.setTitle(&ns(if drawer_open {
            "▾  Recording output"
        } else {
            "▸  Recording output"
        }));
        self.drawer_toggle.setFrame(rect(
            SIDEBAR + 16.0,
            FOOTER + drawer_height - 37.0,
            180.0,
            30.0,
        ));
        self.output_name.setFrame(rect(
            SIDEBAR + 212.0,
            FOOTER + drawer_height - 30.0,
            main_width - 236.0,
            20.0,
        ));
        self.drawer_body.setHidden(!drawer_open);
        self.drawer_body.setFrame(rect(
            SIDEBAR,
            FOOTER,
            main_width,
            DRAWER_OPEN - DRAWER_CLOSED,
        ));
        let field_width = main_width - 48.0;
        self.output_labels[0].setFrame(rect(24.0, 161.0, field_width, 18.0));
        self.directory
            .setFrame(rect(24.0, 131.0, field_width - 112.0, 26.0));
        self.choose
            .setFrame(rect(main_width - 124.0, 129.0, 100.0, 30.0));
        let template_width = field_width - 276.0;
        self.output_labels[1].setFrame(rect(24.0, 102.0, template_width, 18.0));
        self.template
            .setFrame(rect(24.0, 72.0, template_width, 26.0));
        for (index, field) in [&self.width, &self.height, &self.fps]
            .into_iter()
            .enumerate()
        {
            let x = 24.0 + template_width + 20.0 + index as f64 * 88.0;
            self.output_labels[index + 2].setFrame(rect(x, 102.0, 76.0, 18.0));
            field.setFrame(rect(x, 72.0, 76.0, 26.0));
        }
        self.output_hint
            .setFrame(rect(24.0, 45.0, field_width - 140.0, 18.0));
        self.status
            .setFrame(rect(24.0, 10.0, field_width - 140.0, 30.0));
        self.status.setMaximumNumberOfLines(2);
        self.status
            .setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        self.apply
            .setFrame(rect(main_width - 150.0, 15.0, 126.0, 32.0));

        self.footer.setFrame(rect(0.0, 0.0, width, FOOTER));
        self.group_title.setFrame(rect(24.0, 66.0, 200.0, 20.0));
        self.group_reason
            .setFrame(rect(24.0, 43.0, width - 410.0, 20.0));
        self.message.setFrame(rect(24.0, 8.0, width - 48.0, 30.0));
        self.message.setMaximumNumberOfLines(2);
        self.message
            .setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        for (index, button) in self.group_buttons.iter().enumerate() {
            button.setFrame(rect(
                width - 356.0 + index as f64 * 112.0,
                44.0,
                108.0,
                34.0,
            ));
        }
        self.layout_documents();
    }

    fn layout_documents(&self) {
        let rows = self.rows.borrow();
        let list_size = self.list_scroll.contentSize();
        let list_height = list_size.height.max(rows.len() as f64 * ROW_HEIGHT + 8.0);
        let old_list_height = self.list.frame().size.height;
        let old_list_top = self.list_scroll.contentView().bounds().origin.y + list_size.height;
        self.list
            .setFrameSize(NSSize::new(list_size.width, list_height));
        for (index, row) in rows.iter().enumerate() {
            let width = list_size.width - 8.0;
            row.view.setFrame(rect(
                4.0,
                list_height - (index + 1) as f64 * ROW_HEIGHT,
                width,
                ROW_HEIGHT - 6.0,
            ));
            row.background.setFrame(row.view.bounds());
            row.title.setFrame(rect(12.0, 33.0, width - 24.0, 20.0));
            row.detail.setFrame(rect(12.0, 11.0, width - 24.0, 18.0));
            row.select.setFrame(row.view.bounds());
        }
        preserve_scroll_top(
            &self.list_scroll,
            old_list_height - old_list_top,
            list_height,
        );

        let cards = self.cards.borrow();
        let grid_size = self.grid_scroll.contentSize();
        let geometry = GridGeometry::new(grid_size.width, grid_size.height, cards.len());
        let old_height = self.grid.frame().size.height;
        let old_top = self.grid_scroll.contentView().bounds().origin.y + grid_size.height;
        self.grid
            .setFrameSize(NSSize::new(grid_size.width, geometry.document_height));
        for (index, card) in cards.iter().enumerate() {
            let frame = geometry.frame(index);
            let width = frame.size.width;
            let height = frame.size.height;
            let preview_height = (width - 24.0) * 9.0 / 16.0;
            card.view.setFrame(frame);
            card.background.setFrame(card.view.bounds());
            card.preview.view.setFrame(rect(
                12.0,
                height - 12.0 - preview_height,
                width - 24.0,
                preview_height,
            ));
            card.preview.layout();
            card.name.setFrame(rect(16.0, 130.0, width - 104.0, 22.0));
            card.selection
                .setFrame(rect(width - 84.0, 132.0, 68.0, 18.0));
            card.video.setFrame(rect(16.0, 102.0, width - 32.0, 20.0));
            card.audio.setFrame(rect(16.0, 78.0, width - 32.0, 20.0));
            card.report.setFrame(rect(16.0, 8.0, width - 32.0, 64.0));
            card.select.setFrame(rect(0.0, 74.0, width, height - 74.0));
        }
        self.empty_title.setFrame(rect(
            24.0,
            geometry.document_height - grid_size.height / 2.0 + 12.0,
            grid_size.width - 48.0,
            30.0,
        ));
        self.empty_hint.setFrame(rect(
            48.0,
            geometry.document_height - grid_size.height / 2.0 - 40.0,
            grid_size.width - 96.0,
            44.0,
        ));
        preserve_scroll_top(
            &self.grid_scroll,
            old_height - old_top,
            geometry.document_height,
        );
    }
}

fn preserve_scroll_top(scroll: &NSScrollView, top_gap: f64, document_height: f64) {
    let clip = scroll.contentView();
    let maximum = (document_height - clip.bounds().size.height).max(0.0);
    clip.scrollToPoint(NSPoint::new(
        0.0,
        (maximum - top_gap.max(0.0)).clamp(0.0, maximum),
    ));
    scroll.reflectScrolledClipView(&clip);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_adapts_column_count_without_shrinking_cards() {
        assert_eq!(GridGeometry::new(600.0, 400.0, 5).columns, 1);
        assert_eq!(GridGeometry::new(716.0, 400.0, 5).columns, 2);
        assert_eq!(GridGeometry::new(1076.0, 400.0, 5).columns, 3);
        assert_eq!(GridGeometry::new(1436.0, 400.0, 5).columns, 4);
    }

    #[test]
    fn cards_stay_inside_document_and_do_not_overlap() {
        for width in [600.0, 792.0, 992.0, 1400.0, 2000.0] {
            for count in [1, 2, 3, 8, 35] {
                let geometry = GridGeometry::new(width, 350.0, count);
                for index in 0..count {
                    let frame = geometry.frame(index);
                    assert!(frame.origin.x >= 0.0 && frame.origin.y >= 0.0);
                    assert!(frame.origin.x + frame.size.width <= width);
                    assert!(frame.origin.y + frame.size.height <= geometry.document_height);
                    if index >= geometry.columns {
                        let above = geometry.frame(index - geometry.columns);
                        assert!(frame.origin.y + frame.size.height < above.origin.y);
                    }
                }
            }
        }
    }

    #[test]
    fn preview_is_sixteen_by_nine_and_empty_grid_fills_viewport() {
        let geometry = GridGeometry::new(792.0, 350.0, 0);
        assert_eq!(geometry.document_height, 350.0);
        let preview_width = geometry.card_width - 24.0;
        let preview_height = geometry.card_height - 174.0;
        assert!((preview_width / preview_height - 16.0 / 9.0).abs() < 0.0001);
        let populated = GridGeometry::new(792.0, 350.0, 20);
        assert!(populated.document_height > 350.0);
        assert!(populated.frame(0).origin.y > populated.frame(2).origin.y);
    }
}
