//! Clean Metal presentation and a separate, main-thread-only framing overlay.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSSize};

use crate::app::AppState;
use crate::model::{RecordingConfig, RecordingId, Resolution, ViewportTransform};
use crate::render::transform::{fill, fit, geometry, validate};
use crate::render::{PreviewPresenter, RenderedFrame};

use super::{AppDelegate, label, ns, rect};

#[derive(Clone, Copy)]
struct Mapping {
    frame: NSRect,
    canvas: Resolution,
    ratio: f64,
}

impl Mapping {
    fn new(size: NSSize, canvas: Resolution) -> Self {
        let ratio = (size.width / canvas.width.max(1) as f64)
            .min(size.height / canvas.height.max(1) as f64)
            .max(f64::EPSILON);
        let width = canvas.width as f64 * ratio;
        let height = canvas.height as f64 * ratio;
        Self {
            frame: rect(
                (size.width - width) / 2.0,
                (size.height - height) / 2.0,
                width,
                height,
            ),
            canvas,
            ratio,
        }
    }

    fn canvas_point(self, point: NSPoint) -> NSPoint {
        NSPoint::new(
            (point.x - self.frame.origin.x) / self.ratio,
            self.canvas.height as f64 - (point.y - self.frame.origin.y) / self.ratio,
        )
    }

    fn view_rect(self, x: f64, y: f64, width: f64, height: f64) -> NSRect {
        rect(
            self.frame.origin.x + x * self.ratio,
            self.frame.origin.y + (self.canvas.height as f64 - y - height) * self.ratio,
            width * self.ratio,
            height * self.ratio,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Hit {
    Move,
    Corner { right: bool, bottom: bool },
}

#[derive(Clone, Copy)]
struct Context {
    id: RecordingId,
    transform: ViewportTransform,
    source: Resolution,
    canvas: Resolution,
}

fn hit_test(context: Context, point: NSPoint, tolerance: f64) -> Option<Hit> {
    let p = geometry(context.transform, context.source, context.canvas)
        .ok()?
        .placement;
    for right in [false, true] {
        for bottom in [false, true] {
            let x = p.x + if right { p.width } else { 0.0 };
            let y = p.y + if bottom { p.height } else { 0.0 };
            if (point.x - x).abs() <= tolerance && (point.y - y).abs() <= tolerance {
                return Some(Hit::Corner { right, bottom });
            }
        }
    }
    (point.x >= p.x && point.x <= p.x + p.width && point.y >= p.y && point.y <= p.y + p.height)
        .then_some(Hit::Move)
}

#[derive(Clone, Copy)]
struct Drag {
    context: Context,
    start: NSPoint,
    hit: Hit,
    crop: bool,
}

fn dragged(drag: Drag, point: NSPoint) -> Option<ViewportTransform> {
    let Context {
        transform: mut t,
        source,
        canvas,
        ..
    } = drag.context;
    let p = geometry(t, source, canvas).ok()?.placement;
    let dx = point.x - drag.start.x;
    let dy = point.y - drag.start.y;
    match drag.hit {
        Hit::Move => {
            t.offset_x += dx / canvas.width as f64;
            t.offset_y += dy / canvas.height as f64;
        }
        Hit::Corner { right, bottom } if drag.crop => {
            let unit_x = p.width / t.crop_width;
            let unit_y = p.height / t.crop_height;
            let min_w = (1.0 / source.width as f64).min(t.crop_width);
            let min_h = (1.0 / source.height as f64).min(t.crop_height);
            let mut x = p.x;
            let mut y = p.y;
            if right {
                t.crop_width = (t.crop_width + dx / unit_x).clamp(min_w, 1.0 - t.crop_x);
            } else {
                let end = t.crop_x + t.crop_width;
                let next = (t.crop_x + dx / unit_x).clamp(0.0, end - min_w);
                x += (next - t.crop_x) * unit_x;
                t.crop_x = next;
                t.crop_width = end - next;
            }
            if bottom {
                t.crop_height = (t.crop_height + dy / unit_y).clamp(min_h, 1.0 - t.crop_y);
            } else {
                let end = t.crop_y + t.crop_height;
                let next = (t.crop_y + dy / unit_y).clamp(0.0, end - min_h);
                y += (next - t.crop_y) * unit_y;
                t.crop_y = next;
                t.crop_height = end - next;
            }
            let width = t.crop_width * unit_x;
            let height = t.crop_height * unit_y;
            t.scale = 1.0;
            let base = geometry(t, source, canvas).ok()?.placement;
            t.scale = width / base.width;
            t.offset_x = (x + width / 2.0) / canvas.width as f64 - 0.5;
            t.offset_y = (y + height / 2.0) / canvas.height as f64 - 0.5;
        }
        Hit::Corner { right, bottom } => {
            let sx = if right { 1.0 } else { -1.0 };
            let sy = if bottom { 1.0 } else { -1.0 };
            let factor = (1.0
                + (sx * dx * p.width + sy * dy * p.height)
                    / (p.width * p.width + p.height * p.height))
                .max(0.01);
            let scale = (t.scale * factor).clamp(0.01, 100.0);
            let factor = scale / t.scale;
            t.scale = scale;
            t.offset_x += sx * p.width * (factor - 1.0) / (2.0 * canvas.width as f64);
            t.offset_y += sy * p.height * (factor - 1.0) / (2.0 * canvas.height as f64);
        }
    }
    validate(&t).ok()?;
    Some(t)
}

struct OverlayIvars {
    context: Cell<Option<Context>>,
    drag: Cell<Option<Drag>>,
    pending: Cell<Option<(RecordingId, ViewportTransform)>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = OverlayIvars]
    struct ViewportOverlay;

    unsafe impl NSObjectProtocol for ViewportOverlay {}

    impl ViewportOverlay {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool { true }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let Some(context) = self.ivars().context.get() else { return; };
            let Ok(g) = geometry(context.transform, context.source, context.canvas) else { return; };
            let map = Mapping::new(self.bounds().size, context.canvas);
            let p = g.placement;
            let frame = map.view_rect(p.x, p.y, p.width, p.height);
            NSColor::controlAccentColor().setStroke();
            let path = NSBezierPath::bezierPathWithRect(frame);
            path.setLineWidth(2.0);
            path.stroke();
            for x in [frame.origin.x, frame.origin.x + frame.size.width] {
                for y in [frame.origin.y, frame.origin.y + frame.size.height] {
                    let handle = NSBezierPath::bezierPathWithRect(rect(x - 4.0, y - 4.0, 8.0, 8.0));
                    NSColor::controlBackgroundColor().setFill();
                    handle.fill();
                    handle.stroke();
                }
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let Some(context) = self.ivars().context.get() else { return; };
            let map = Mapping::new(self.bounds().size, context.canvas);
            let point = map.canvas_point(self.convertPoint_fromView(event.locationInWindow(), None));
            self.ivars().drag.set(hit_test(context, point, 10.0 / map.ratio).map(|hit| Drag {
                context, start: point, hit,
                crop: event.modifierFlags().contains(NSEventModifierFlags::Option),
            }));
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let Some(drag) = self.ivars().drag.get() else { return; };
            let point = Mapping::new(self.bounds().size, drag.context.canvas)
                .canvas_point(self.convertPoint_fromView(event.locationInWindow(), None));
            if let Some(transform) = dragged(drag, point) { self.edit(transform); }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) { self.ivars().drag.set(None); }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let Some(context) = self.ivars().context.get() else { return; };
            let mut transform = context.transform;
            transform.scale = (transform.scale * (-event.scrollingDeltaY() * 0.01).exp()).clamp(0.01, 100.0);
            if validate(&transform).is_ok() { self.edit(transform); }
        }
    }
);

impl ViewportOverlay {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(OverlayIvars {
            context: Cell::new(None),
            drag: Cell::new(None),
            pending: Cell::new(None),
        });
        unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, 1.0, 1.0)] }
    }

    fn edit(&self, transform: ViewportTransform) {
        if let Some(mut context) = self.ivars().context.get() {
            context.transform = transform;
            self.ivars().context.set(Some(context));
            self.ivars().pending.set(Some((context.id, transform)));
            self.setNeedsDisplay(true);
        }
    }
}

#[derive(PartialEq)]
struct PresentationSignature {
    timestamp: f64,
    transform: ViewportTransform,
    canvas: Resolution,
    size: NSSize,
    backing: f64,
}

pub(super) struct Preview {
    pub view: Retained<NSView>,
    surface: Retained<NSView>,
    message: Retained<NSTextField>,
    presenter: RefCell<Result<PreviewPresenter, String>>,
    overlay: Option<Retained<ViewportOverlay>>,
    source: Cell<Option<Resolution>>,
    canvas: Cell<Resolution>,
    last: RefCell<Option<PresentationSignature>>,
}

impl Preview {
    pub fn new(mtm: MainThreadMarker, editing: bool) -> Self {
        let view = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, 1.0, 1.0));
        let background = NSBox::initWithFrame(NSBox::alloc(mtm), view.bounds());
        background.setBoxType(NSBoxType::Custom);
        background.setTitlePosition(NSTitlePosition::NoTitle);
        background.setFillColor(&NSColor::blackColor());
        background.setBorderWidth(0.0);
        background.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        view.addSubview(&background);
        let surface = NSView::initWithFrame(NSView::alloc(mtm), view.bounds());
        let presenter = PreviewPresenter::new();
        if let Ok(presenter) = &presenter {
            surface.setLayer(Some(presenter.layer()));
            surface.setWantsLayer(true);
        }
        view.addSubview(&surface);
        let message = label(&view, "Waiting for video…", view.bounds(), mtm);
        message.setTextColor(Some(&NSColor::whiteColor()));
        message.setAlignment(NSTextAlignment::Center);
        message.setFont(Some(&NSFont::systemFontOfSize(12.0)));
        let overlay = editing.then(|| {
            let overlay = ViewportOverlay::new(mtm);
            view.addSubview(&overlay);
            overlay
        });
        Self {
            view,
            surface,
            message,
            presenter: RefCell::new(presenter),
            overlay,
            source: Cell::new(None),
            canvas: Cell::new(Resolution::default()),
            last: RefCell::new(None),
        }
    }

    pub fn visible(&self) -> bool {
        let visible = self.view.visibleRect().size;
        !self.view.isHiddenOrHasHiddenAncestor()
            && visible.width > 0.0
            && visible.height > 0.0
            && self.view.window().is_some_and(|w| {
                w.isVisible()
                    && !w.isMiniaturized()
                    && w.occlusionState().contains(NSWindowOcclusionState::Visible)
            })
    }

    pub fn layout(&self) {
        let size = self.view.bounds().size;
        let frame = Mapping::new(size, self.canvas.get()).frame;
        if self.surface.frame() != frame {
            self.surface.setFrame(frame);
        }
        self.message.setFrame(rect(
            16.0,
            (size.height / 2.0 - 36.0).max(0.0),
            (size.width - 32.0).max(0.0),
            72.0,
        ));
        if let Some(overlay) = &self.overlay
            && overlay.frame() != self.view.bounds()
        {
            overlay.setFrame(self.view.bounds());
            overlay.setNeedsDisplay(true);
        }
    }

    pub fn clear(&self, message: &str) {
        self.surface.setHidden(true);
        self.message.setHidden(false);
        self.message.setStringValue(&ns(message));
        self.source.set(None);
        self.last.borrow_mut().take();
        if let Some(overlay) = &self.overlay {
            overlay.ivars().context.set(None);
            overlay.ivars().drag.set(None);
            overlay.setNeedsDisplay(true);
        }
        self.layout();
    }

    pub fn update(
        &self,
        recording: &RecordingConfig,
        frame: Option<Result<RenderedFrame, String>>,
        status: &str,
        editable: bool,
    ) {
        self.canvas.set(recording.output.resolution);
        self.layout();
        let frame = match frame {
            Some(Ok(frame)) => frame,
            Some(Err(error)) => {
                self.clear(&format!("Preview failed: {error}. Try Refresh Sources."));
                return;
            }
            None => {
                self.clear(status);
                return;
            }
        };
        self.source.set(Some(frame.source_size));
        if let Some(overlay) = &self.overlay {
            overlay.ivars().context.set(editable.then_some(Context {
                id: recording.id,
                transform: recording.viewport,
                source: frame.source_size,
                canvas: recording.output.resolution,
            }));
            overlay.setNeedsDisplay(true);
        }
        // Keep the last clean frame visible while live gestures queue their next
        // transform. Only a changed canvas invalidates its presentation geometry.
        if frame.canvas != recording.output.resolution {
            self.surface.setHidden(true);
            self.message.setHidden(false);
            self.message.setStringValue(&ns("Updating framing…"));
            self.last.borrow_mut().take();
            return;
        }
        let size = self.surface.bounds().size;
        let backing = self
            .view
            .window()
            .map(|w| w.backingScaleFactor())
            .unwrap_or(1.0);
        let signature = PresentationSignature {
            timestamp: frame.timestamp_seconds,
            transform: frame.transform,
            canvas: frame.canvas,
            size,
            backing,
        };
        if self.last.borrow().as_ref() == Some(&signature) {
            return;
        }
        let result = match &mut *self.presenter.borrow_mut() {
            Ok(presenter) => presenter.present(&frame, size.width, size.height, backing),
            Err(error) => Err(error.clone()),
        };
        match result {
            Ok(()) => {
                self.surface.setHidden(false);
                self.message.setHidden(true);
                *self.last.borrow_mut() = Some(signature);
            }
            Err(error) => self.clear(&format!("Cannot present preview: {error}")),
        }
    }

    fn take_edit(&self) -> Option<(RecordingId, ViewportTransform)> {
        self.overlay.as_ref()?.ivars().pending.take()
    }

    pub fn smoke_check_layer(&self) {
        let presenter = self.presenter.borrow();
        let presenter = presenter
            .as_ref()
            .unwrap_or_else(|error| panic!("Preview presenter: {error}"));
        let layer = self
            .surface
            .layer()
            .expect("CAMetalLayer must be attached to presentation subview");
        assert_eq!(&*layer, &**presenter.layer());
        if let Some(overlay) = &self.overlay {
            let parent = unsafe { overlay.superview() };
            assert_eq!(parent.as_deref(), Some(&*self.view));
            assert_ne!(parent.as_deref(), Some(&*self.surface));
        }
    }

    pub fn smoke_check_presented(&self) {
        self.smoke_check_layer();
        assert!(
            self.last.borrow().is_some(),
            "No successful Metal presentation: {}",
            self.message.stringValue()
        );
        assert!(!self.surface.isHidden());
        assert!(self.message.isHidden());
        if let Some(overlay) = &self.overlay {
            assert!(overlay.ivars().context.get().is_some());
            assert!(!overlay.isHidden());
        }
    }

    pub fn smoke_show_handles(&self, visible: bool) {
        self.overlay.as_ref().unwrap().setHidden(!visible);
    }

    pub fn smoke_gpu_ready(&self) -> bool {
        self.visible()
            && self.last.borrow().is_some()
            && self
                .presenter
                .borrow()
                .as_ref()
                .is_ok_and(|p| p.presented_frames() > 0)
    }

    pub fn smoke_check_gpu_completion(&self) {
        assert!(
            self.presenter.borrow().as_ref().unwrap().presented_frames() > 0,
            "No native display-link GPU submission completed"
        );
    }

    pub fn smoke_check_orientation(&self, screenshot: &std::path::Path) {
        self.smoke_check_gpu_completion();
        let data = objc2_foundation::NSData::with_bytes(&std::fs::read(screenshot).unwrap());
        let bitmap = NSBitmapImageRep::imageRepWithData(&data).expect("composited window bitmap");
        let window = self.surface.window().unwrap();
        let scale = bitmap.pixelsWide() as f64 / window.frame().size.width;
        let bounds = self.surface.frame();
        // The synthetic 80x40 source fits a 160x120 canvas with letterboxing.
        // Sample inside each quadrant, away from borders and sibling handles.
        for (name, x, y, expected) in [
            ("top-left", 0.25, 0.625, [1.0, 0.0, 0.0]),
            ("top-right", 0.75, 0.625, [0.0, 1.0, 0.0]),
            ("bottom-left", 0.25, 0.375, [0.0, 0.0, 1.0]),
            ("bottom-right", 0.75, 0.375, [1.0, 1.0, 1.0]),
        ] {
            let point = self.view.convertPoint_toView(
                NSPoint::new(
                    bounds.origin.x + bounds.size.width * x,
                    bounds.origin.y + bounds.size.height * y,
                ),
                None,
            );
            let pixel_x = (point.x * scale).floor() as isize;
            let pixel_y = (bitmap.pixelsHigh() as f64 - point.y * scale).floor() as isize;
            assert!((0..bitmap.pixelsWide()).contains(&pixel_x));
            assert!((0..bitmap.pixelsHigh()).contains(&pixel_y));
            let color = bitmap
                .colorAtX_y(pixel_x, pixel_y)
                .expect("screenshot quadrant pixel")
                .colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())
                .expect("screenshot RGB color");
            let actual = [
                color.redComponent(),
                color.greenComponent(),
                color.blueComponent(),
            ];
            // WindowServer applies the display's color profile. Check distinct
            // quadrant colors, not byte identity with the uncomposited buffer.
            assert!(
                actual
                    .iter()
                    .zip(expected)
                    .all(|(a, e)| { if e == 1.0 { *a > 0.8 } else { *a < 0.55 } }),
                "{} {name} at ({pixel_x}, {pixel_y}): RGB {actual:?}, expected {expected:?}",
                screenshot.display()
            );
            println!("Composited {name}: RGB {actual:?}");
        }
        println!("Composited orientation PASS: {}", screenshot.display());
    }

    fn smoke_mouse_event(&self, kind: NSEventType) {
        let overlay = self.overlay.as_ref().unwrap();
        let context = overlay.ivars().context.get().unwrap();
        let p = geometry(context.transform, context.source, context.canvas)
            .unwrap()
            .placement;
        let frame = Mapping::new(overlay.bounds().size, context.canvas)
            .view_rect(p.x, p.y, p.width, p.height);
        let point = NSPoint::new(
            frame.origin.x + frame.size.width / 2.0,
            frame.origin.y + frame.size.height / 2.0,
        );
        let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                kind, overlay.convertPoint_toView(point, None), NSEventModifierFlags::empty(),
                0.0, self.view.window().unwrap().windowNumber(), None, 1, 1, 1.0,
            )
            .expect("native framing mouse event");
        match kind {
            NSEventType::LeftMouseDown => overlay.mouseDown(&event),
            NSEventType::LeftMouseUp => overlay.mouseUp(&event),
            _ => unreachable!(),
        }
    }
}

pub(super) struct Framing {
    pub window: Retained<NSWindow>,
    pub preview: Preview,
    pub fields: Vec<Retained<NSTextField>>,
    pub error: Retained<NSTextField>,
    pub id: Cell<Option<RecordingId>>,
    buttons: Vec<Retained<NSButton>>,
    fill_button: Retained<NSButton>,
}

impl Framing {
    pub fn new(delegate: &AppDelegate) -> Self {
        let mtm = delegate.mtm();
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect(0.0, 0.0, 920.0, 760.0),
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Resizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe {
            window.setReleasedWhenClosed(false);
        }
        window.setTitle(&ns("Framing"));
        window.setContentMinSize(NSSize::new(920.0, 650.0));
        window.setDelegate(Some(ProtocolObject::from_ref(delegate)));
        let root = window.contentView().expect("framing content");
        let preview = Preview::new(mtm, true);
        preview.view.setFrame(rect(24.0, 250.0, 872.0, 486.0));
        preview.view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        root.addSubview(&preview.view);
        label(
            &root,
            "Drag to move · Drag corners to resize · Option-drag corners to crop · Scroll to zoom",
            rect(24.0, 215.0, 872.0, 24.0),
            mtm,
        );
        let mut fields = Vec::new();
        for (index, title) in [
            "Crop X %",
            "Crop Y %",
            "Crop width %",
            "Crop height %",
            "Offset X %",
            "Offset Y %",
            "Scale %",
        ]
        .into_iter()
        .enumerate()
        {
            let x = 24.0 + index as f64 * 124.0;
            label(&root, title, rect(x, 181.0, 116.0, 20.0), mtm);
            let field = NSTextField::textFieldWithString(&ns(""), mtm);
            field.setFrame(rect(x, 148.0, 114.0, 28.0));
            field.setAccessibilityLabel(Some(&ns(title)));
            unsafe {
                field.setTarget(Some(delegate));
                field.setAction(Some(sel!(applyFraming:)));
            }
            root.addSubview(&field);
            fields.push(field);
        }
        label(
            &root,
            "Crop is normalized to the source. Offsets translate from centered Fit. Output canvas stays fixed.",
            rect(24.0, 112.0, 872.0, 24.0),
            mtm,
        );
        let mut buttons = Vec::new();
        for (index, (title, action)) in [
            ("Apply Framing", sel!(applyFraming:)),
            ("Fit", sel!(fitFraming:)),
            ("Fill", sel!(fillFraming:)),
            ("Reset", sel!(resetFraming:)),
        ]
        .into_iter()
        .enumerate()
        {
            buttons.push(delegate.button(
                &root,
                title,
                rect(24.0 + index as f64 * 146.0, 66.0, 138.0, 32.0),
                action,
            ));
        }
        let fill_button = buttons[2].clone();
        let error = label(&root, "", rect(24.0, 12.0, 872.0, 44.0), mtm);
        error.setTextColor(Some(&NSColor::secondaryLabelColor()));
        Self {
            window,
            preview,
            fields,
            error,
            id: Cell::new(None),
            buttons,
            fill_button,
        }
    }

    pub fn show(&self, recording: &RecordingConfig, editable: bool) {
        self.window.makeFirstResponder(None);
        self.id.set(Some(recording.id));
        self.preview
            .clear("Waiting for video. Choose a source or check permissions in Settings.");
        self.preview.canvas.set(recording.output.resolution);
        self.window
            .setTitle(&ns(&format!("Framing — {}", recording.name)));
        self.write_fields(recording.viewport);
        self.set_enabled(editable);
        self.error.setStringValue(&ns(
            "Changes apply live and save automatically. Fit preserves crop; Reset removes it.",
        ));
        self.window.center();
        self.window.makeKeyAndOrderFront(None);
    }

    fn set_enabled(&self, enabled: bool) {
        for field in &self.fields {
            field.setEnabled(enabled);
        }
        for button in &self.buttons {
            button.setEnabled(enabled);
        }
        self.fill_button
            .setEnabled(enabled && self.preview.source.get().is_some());
    }

    pub fn write_fields(&self, t: ViewportTransform) {
        for (field, value) in self.fields.iter().zip([
            t.crop_x,
            t.crop_y,
            t.crop_width,
            t.crop_height,
            t.offset_x,
            t.offset_y,
            t.scale,
        ]) {
            field.setStringValue(&ns(&format!("{:.6}", value * 100.0)));
        }
    }

    pub fn numeric(&self) -> Result<ViewportTransform, String> {
        let values: Result<Vec<f64>, _> = self
            .fields
            .iter()
            .map(|f| {
                f.stringValue()
                    .to_string()
                    .trim()
                    .parse::<f64>()
                    .map(|v| v / 100.0)
            })
            .collect();
        let v = values.map_err(|_| "Enter finite numbers in every framing field.".to_string())?;
        let t = ViewportTransform {
            crop_x: v[0],
            crop_y: v[1],
            crop_width: v[2],
            crop_height: v[3],
            offset_x: v[4],
            offset_y: v[5],
            scale: v[6],
        };
        validate(&t)?;
        Ok(t)
    }
}

pub(super) fn persist_edit(
    state: &mut AppState,
    id: RecordingId,
    transform: ViewportTransform,
) -> Result<bool, String> {
    if !state.can_save {
        return Err("Repair settings before editing framing.".into());
    }
    validate(&transform)?;
    let recording = state
        .settings
        .recordings
        .iter_mut()
        .find(|r| r.id == id)
        .ok_or_else(|| "This source was removed. Choose another source.".to_string())?;
    if recording.viewport == transform {
        return Ok(false);
    }
    recording.viewport = transform;
    state.dirty = true;
    Ok(true)
}

impl AppDelegate {
    pub(super) fn smoke_start_presentation(&self) {
        let frame = crate::render::synthetic_preview_frame().expect("synthetic rendered frame");
        let c = self.controls();
        c.width.setStringValue(&ns(&frame.canvas.width.to_string()));
        c.height
            .setStringValue(&ns(&frame.canvas.height.to_string()));
        self.smoke_click(sel!(applyChanges:));
        let id = self.ivars().state.borrow().selected.unwrap();
        self.apply_viewport_edit(id, frame.transform).unwrap();
        *self.ivars().smoke_preview.borrow_mut() = Some((id, frame));
        unsafe {
            c.workspace.framing.performClick(None);
        }
    }

    pub(super) fn smoke_begin_presentation_drag(&self) {
        let c = self.controls();
        self.poll_previews();
        c.framing
            .preview
            .smoke_mouse_event(NSEventType::LeftMouseDown);
        assert!(
            c.framing
                .preview
                .overlay
                .as_ref()
                .unwrap()
                .ivars()
                .drag
                .get()
                .is_some()
        );
    }

    pub(super) fn smoke_end_presentation_drag(&self) {
        let preview = &self.controls().framing.preview;
        assert!(
            preview
                .overlay
                .as_ref()
                .unwrap()
                .ivars()
                .drag
                .get()
                .is_some()
        );
        preview.smoke_mouse_event(NSEventType::LeftMouseUp);
        assert!(
            preview
                .overlay
                .as_ref()
                .unwrap()
                .ivars()
                .drag
                .get()
                .is_none()
        );
    }

    pub(super) fn smoke_check_presentation(&self) {
        let id = self.ivars().smoke_preview.borrow().as_ref().unwrap().0;
        self.controls().workspace.smoke_check_presented(id);
        self.controls().framing.preview.smoke_check_presented();
    }

    pub(super) fn smoke_check_canvas_change(&self) {
        let c = self.controls();
        let mut pending = self.ivars().state.borrow().selected().unwrap().clone();
        pending.viewport.offset_x += 0.01;
        let frame = self
            .ivars()
            .smoke_preview
            .borrow()
            .as_ref()
            .unwrap()
            .1
            .clone();
        c.framing
            .preview
            .update(&pending, Some(Ok(frame)), "", true);
        assert!(
            !c.framing.preview.surface.isHidden(),
            "Live framing edits must retain video while the render queue catches up"
        );
        let original = self
            .ivars()
            .state
            .borrow()
            .selected()
            .unwrap()
            .output
            .resolution;
        let canvas = Resolution {
            width: original.width + 2,
            height: original.height + 2,
        };
        c.width.setStringValue(&ns(&canvas.width.to_string()));
        c.height.setStringValue(&ns(&canvas.height.to_string()));
        self.smoke_click(sel!(applyChanges:));
        assert_eq!(
            self.ivars()
                .state
                .borrow()
                .selected()
                .unwrap()
                .output
                .resolution,
            canvas,
            "Canvas edit failed: {}",
            self.ivars().state.borrow().message,
        );
        self.poll_previews();
        let preview = &c.framing.preview;
        assert_eq!(preview.canvas.get(), canvas);
        assert_eq!(
            preview
                .overlay
                .as_ref()
                .unwrap()
                .ivars()
                .context
                .get()
                .unwrap()
                .canvas,
            canvas
        );
        assert!(
            preview.surface.isHidden(),
            "Stale output canvas must not be stretched"
        );
        assert_eq!(
            preview.message.stringValue().to_string(),
            "Updating framing…"
        );
        c.width.setStringValue(&ns(&original.width.to_string()));
        c.height.setStringValue(&ns(&original.height.to_string()));
        self.smoke_click(sel!(applyChanges:));
        self.poll_previews();
        self.smoke_check_presentation();
    }

    pub(super) fn flush_viewport_edit(&self) {
        let framing = &self.controls().framing;
        if let Some((id, transform)) = framing.preview.take_edit() {
            if let Err(error) = self.apply_viewport_edit(id, transform) {
                framing.error.setStringValue(&ns(&error));
            } else {
                framing.write_fields(transform);
            }
        }
    }
    pub(super) fn smoke_framing(&self) {
        let c = self.controls();
        let first = self.ivars().state.borrow().settings.recordings[0].clone();
        let second = self.ivars().state.borrow().settings.recordings[1].clone();
        unsafe {
            c.workspace.framing.performClick(None);
        }
        assert!(c.framing.window.isVisible());
        assert_eq!(c.framing.id.get(), Some(first.id));
        c.framing.preview.smoke_check_layer();
        c.workspace.smoke_check_layers();
        c.framing.fields[6].setStringValue(&ns("NaN"));
        self.smoke_click(sel!(applyFraming:));
        assert_eq!(
            self.ivars().state.borrow().settings.recordings[0].viewport,
            first.viewport
        );
        for (field, value) in c
            .framing
            .fields
            .iter()
            .zip(["10", "5", "80", "90", "-10", "5", "125"])
        {
            field.setStringValue(&ns(value));
        }
        self.smoke_click(sel!(applyFraming:));
        let edited = self.ivars().state.borrow().settings.recordings[0].viewport;
        assert_eq!(edited.crop_x, 0.1);
        assert_eq!(edited.scale, 1.25);
        assert_eq!(self.ivars().state.borrow().settings.recordings[1], second);
        self.smoke_click(sel!(fitFraming:));
        let fitted = self.ivars().state.borrow().settings.recordings[0].viewport;
        assert_eq!(fitted.crop_width, edited.crop_width);
        assert_eq!(fitted.scale, 1.0);
        assert_eq!(fitted.offset_x, 0.0);
        self.smoke_click(sel!(resetFraming:));
        assert_eq!(
            self.ivars().state.borrow().settings.recordings[0].viewport,
            ViewportTransform::default()
        );
        c.framing.fields[6].setStringValue(&ns("140"));
        self.smoke_click(sel!(applyFraming:));
        c.framing.window.performClose(None);
        assert!(!c.framing.window.isVisible());
        assert!(c.window.isVisible());
        unsafe {
            c.workspace.framing.performClick(None);
        }
        assert_eq!(c.framing.numeric().unwrap().scale, 1.4);
        c.framing.window.performClose(None);
        assert!(
            c.workspace
                .group_buttons
                .iter()
                .all(|button| !button.isEnabled())
        );
    }

    pub(super) fn edit_framing(&self, operation: &str) {
        let framing = &self.controls().framing;
        let Some(id) = framing.id.get() else {
            return;
        };
        let result = (|| {
            let mut t = if operation == "reset" {
                ViewportTransform::default()
            } else {
                framing.numeric()?
            };
            match operation {
                "fit" => fit(&mut t),
                "fill" => {
                    let source = framing
                        .preview
                        .source
                        .get()
                        .ok_or("Wait for a live source before using Fill.")?;
                    let canvas = self
                        .ivars()
                        .state
                        .borrow()
                        .settings
                        .recordings
                        .iter()
                        .find(|r| r.id == id)
                        .map(|r| r.output.resolution)
                        .ok_or("Source was removed.")?;
                    fill(&mut t, source, canvas)?;
                }
                _ => {}
            }
            self.apply_viewport_edit(id, t)?;
            framing.write_fields(t);
            Ok::<(), String>(())
        })();
        framing.error.setStringValue(&ns(&result
            .err()
            .unwrap_or_else(|| "Framing applied. Changes save automatically.".into())));
    }

    fn apply_viewport_edit(
        &self,
        id: RecordingId,
        transform: ViewportTransform,
    ) -> Result<(), String> {
        if persist_edit(&mut self.ivars().state.borrow_mut(), id, transform)? {
            self.ivars()
                .viewport_edited
                .set(Some(std::time::Instant::now()));
        }
        Ok(())
    }

    pub(super) fn poll_previews(&self) {
        let c = self.controls();
        self.flush_viewport_edit();
        if self.ivars().smoke_preview.borrow().is_some() {
            self.ivars()
                .smoke_preview_ticks
                .set(self.ivars().smoke_preview_ticks.get() + 1);
            if objc2_foundation::NSRunLoop::mainRunLoop()
                .currentMode()
                .as_deref()
                == Some(unsafe { NSEventTrackingRunLoopMode })
            {
                self.ivars()
                    .smoke_tracking_ticks
                    .set(self.ivars().smoke_tracking_ticks.get() + 1);
            }
        }
        let state = self.ivars().state.borrow();
        if c.framing
            .id
            .get()
            .is_some_and(|id| !state.settings.recordings.iter().any(|r| r.id == id))
        {
            c.framing.id.set(None);
            c.framing.window.orderOut(None);
            c.framing.preview.clear("Source removed.");
        }
        let foreground = NSApplication::sharedApplication(self.mtm()).isActive();
        let mut configs = Vec::new();
        for recording in &state.settings.recordings {
            let editor_visible =
                c.framing.id.get() == Some(recording.id) && c.framing.preview.visible();
            let visible = c.workspace.preview_visible(recording.id) || editor_visible;
            if let Some(source) = crate::capture::SourceKey::video(&recording.video) {
                configs.push(crate::platform::macos::capture::PreviewConfig {
                    recording: recording.id,
                    source,
                    transform: recording.viewport,
                    canvas: recording.output.resolution,
                    fps: recording.output.fps,
                    active: foreground && (state.selected == Some(recording.id) || editor_visible),
                    visible,
                });
            }
            let status = if matches!(recording.video, crate::model::VideoSourceSelector::None) {
                "Choose a video source using Edit… to start preview.".to_string()
            } else {
                format!(
                    "{}\nWaiting for video. Check Settings permissions or Refresh Sources.",
                    state.capture_summary(recording.id)
                )
            };
            // Only the deliberate native smoke phase supplies a synthetic renderer
            // output; visibility, layout, presentation and overlays remain real.
            let frame = self
                .ivars()
                .smoke_preview
                .borrow()
                .as_ref()
                .filter(|(id, _)| *id == recording.id)
                .map(|(_, frame)| {
                    let mut frame = frame.clone();
                    frame.timestamp_seconds += self.ivars().smoke_preview_ticks.get() as f64 / 30.0;
                    Ok(frame)
                })
                .or_else(|| self.ivars().capture.borrow().preview(recording.id));
            c.workspace
                .update_preview(recording, frame.clone(), &status);
            if c.framing.id.get() == Some(recording.id) && editor_visible {
                c.framing
                    .preview
                    .update(recording, frame, &status, state.can_save);
                c.framing.set_enabled(state.can_save);
            }
        }
        self.ivars().capture.borrow().configure_previews(configs);
        if self.ivars().smoke_preview.borrow().is_some()
            && objc2_foundation::NSRunLoop::mainRunLoop()
                .currentMode()
                .as_deref()
                == Some(unsafe { NSEventTrackingRunLoopMode })
        {
            self.smoke_check_presentation();
            assert!(
                c.framing
                    .preview
                    .overlay
                    .as_ref()
                    .unwrap()
                    .ivars()
                    .drag
                    .get()
                    .is_some()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> Context {
        Context {
            id: RecordingConfig::new("Test").id,
            transform: ViewportTransform::default(),
            source: Resolution {
                width: 1920,
                height: 1080,
            },
            canvas: Resolution {
                width: 1000,
                height: 1000,
            },
        }
    }

    #[test]
    fn coordinates_account_for_letterbox_and_bottom_left_points() {
        let map = Mapping::new(
            NSSize::new(800.0, 600.0),
            Resolution {
                width: 1920,
                height: 1080,
            },
        );
        assert_eq!(
            map.canvas_point(NSPoint::new(0.0, 525.0)),
            NSPoint::new(0.0, 0.0)
        );
        assert_eq!(
            map.canvas_point(NSPoint::new(800.0, 75.0)),
            NSPoint::new(1920.0, 1080.0)
        );
        // NSEvent locations are points on both Retina and non-Retina displays.
        assert_eq!(
            map.canvas_point(NSPoint::new(400.0, 300.0)),
            NSPoint::new(960.0, 540.0)
        );
    }

    #[test]
    fn corners_take_priority_and_letterbox_is_not_draggable() {
        let c = context();
        assert_eq!(
            hit_test(c, NSPoint::new(0.0, 218.75), 10.0),
            Some(Hit::Corner {
                right: false,
                bottom: false
            })
        );
        assert_eq!(
            hit_test(c, NSPoint::new(500.0, 500.0), 10.0),
            Some(Hit::Move)
        );
        assert_eq!(hit_test(c, NSPoint::new(500.0, 20.0), 10.0), None);
    }

    #[test]
    fn resize_keeps_aspect_and_opposite_corner() {
        for right in [false, true] {
            for bottom in [false, true] {
                let c = context();
                let old = geometry(c.transform, c.source, c.canvas).unwrap().placement;
                let t = dragged(
                    Drag {
                        context: c,
                        start: NSPoint::new(0.0, 0.0),
                        hit: Hit::Corner { right, bottom },
                        crop: false,
                    },
                    NSPoint::new(200.0, 70.0),
                )
                .unwrap();
                let p = geometry(t, c.source, c.canvas).unwrap().placement;
                assert!((p.width / p.height - old.width / old.height).abs() < 1e-9);
                assert!(
                    (p.x + if right { 0.0 } else { p.width }
                        - old.x
                        - if right { 0.0 } else { old.width })
                    .abs()
                        < 1e-8
                );
                assert!(
                    (p.y + if bottom { 0.0 } else { p.height }
                        - old.y
                        - if bottom { 0.0 } else { old.height })
                    .abs()
                        < 1e-8
                );
            }
        }
    }

    #[test]
    fn crop_clamps_to_source_and_preserves_pixel_scale() {
        let c = context();
        let old = geometry(c.transform, c.source, c.canvas).unwrap();
        let drag = Drag {
            context: c,
            start: NSPoint::new(0.0, 0.0),
            hit: Hit::Corner {
                right: false,
                bottom: false,
            },
            crop: true,
        };
        for point in [
            NSPoint::new(100.0, 100.0),
            NSPoint::new(5000.0, 5000.0),
            NSPoint::new(-5000.0, -5000.0),
        ] {
            let t = dragged(drag, point).unwrap();
            assert!(t.is_crop_valid());
            let g = geometry(t, c.source, c.canvas).unwrap();
            assert!(
                (g.placement.width / g.crop.width - old.placement.width / old.crop.width).abs()
                    < 1e-8
            );
            assert!(
                (g.placement.x + g.placement.width - old.placement.x - old.placement.width).abs()
                    < 1e-8
            );
        }
    }

    #[test]
    fn edits_use_stable_ids_preserve_sources_and_do_not_reconcile_capture() {
        let mut state = AppState::from_settings(Default::default());
        state.add();
        state.duplicate();
        for recording in &mut state.settings.recordings {
            recording.video = crate::model::VideoSourceSelector::Camera {
                device_id: crate::model::DeviceId("shared-test-camera".into()),
                label: "Test Camera".into(),
            };
            recording.video_availability = crate::model::SourceAvailability::Available;
        }
        let a = state.settings.recordings[0].clone();
        let b = state.settings.recordings[1].clone();
        let before = state.capture_targets();
        assert_eq!(before.len(), 2);
        state.dirty = false;
        let t = ViewportTransform {
            scale: 1.5,
            offset_x: 0.1,
            ..Default::default()
        };
        assert!(persist_edit(&mut state, a.id, t).unwrap());
        assert!(state.dirty);
        assert_eq!(state.settings.recordings[1], b);
        assert_eq!(state.settings.recordings[0].video, a.video);
        assert_eq!(state.capture_targets(), before);
        let encoded = serde_json::to_string(&state.settings).unwrap();
        let restored: crate::storage::PersistedSettings = serde_json::from_str(&encoded).unwrap();
        assert_eq!(restored.recordings[0].viewport, t);
        assert_eq!(restored.recordings[1].viewport, b.viewport);
        state.remove_confirmed(a.id);
        assert!(persist_edit(&mut state, a.id, t).is_err());
        assert_eq!(state.settings.recordings[0].viewport, b.viewport);
    }

    #[test]
    fn framing_stays_editable_during_every_active_recording_phase() {
        use crate::model::GroupState;
        use crate::recording::coordinator::GroupSnapshot;
        for group in [
            GroupState::Preparing,
            GroupState::Recording,
            GroupState::Paused,
            GroupState::Finalizing,
        ] {
            let mut state = AppState::from_settings(Default::default());
            state.add();
            state.duplicate();
            let first = state.settings.recordings[0].clone();
            let second = state.settings.recordings[1].clone();
            state.update_group(GroupSnapshot {
                state: group,
                ..Default::default()
            });
            state.selected = Some(second.id);
            let transform = ViewportTransform {
                scale: 1.5,
                ..Default::default()
            };
            assert!(state.configuration_locked());
            assert!(persist_edit(&mut state, first.id, transform).unwrap());
            assert_eq!(state.settings.recordings[0].viewport, transform);
            assert_eq!(state.settings.recordings[0].output, first.output);
            assert_eq!(state.settings.recordings[1], second);
        }
    }

    #[test]
    fn moving_is_normalized_to_fixed_canvas_not_source_size() {
        let c = context();
        let t = dragged(
            Drag {
                context: c,
                start: NSPoint::new(500.0, 500.0),
                hit: Hit::Move,
                crop: false,
            },
            NSPoint::new(600.0, 450.0),
        )
        .unwrap();
        assert_eq!(t.offset_x, 0.1);
        assert_eq!(t.offset_y, -0.05);
        assert_eq!(t.scale, 1.0);
        assert_eq!(t.crop_width, 1.0);
    }

    #[test]
    fn crop_handles_allow_subpixel_numeric_crops_without_panicking() {
        let mut c = context();
        c.transform.crop_width = 0.0001;
        c.transform.crop_height = 0.0001;
        for right in [false, true] {
            for bottom in [false, true] {
                for delta in [-1000.0, 1000.0] {
                    let t = dragged(
                        Drag {
                            context: c,
                            start: NSPoint::new(0.0, 0.0),
                            hit: Hit::Corner { right, bottom },
                            crop: true,
                        },
                        NSPoint::new(delta, delta),
                    )
                    .unwrap();
                    assert!(validate(&t).is_ok());
                }
            }
        }
    }

    #[test]
    fn invalid_or_readonly_edits_do_not_dirty_settings() {
        let mut state = AppState::from_settings(Default::default());
        state.add();
        let id = state.selected.unwrap();
        state.dirty = false;
        for t in [
            ViewportTransform {
                scale: f64::NAN,
                ..Default::default()
            },
            ViewportTransform {
                crop_width: 0.0,
                ..Default::default()
            },
        ] {
            assert!(persist_edit(&mut state, id, t).is_err());
            assert!(!state.dirty);
        }
        state.can_save = false;
        assert!(persist_edit(&mut state, id, ViewportTransform::default()).is_err());
        assert!(!state.dirty);
    }
}
