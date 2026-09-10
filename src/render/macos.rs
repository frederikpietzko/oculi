use super::transform::{self, Geometry};
use crate::model::{Resolution, ViewportTransform};
use crate::platform::macos::sample::NativeSample;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CGAffineTransform, CGPoint,
    CGRect, CGSize,
};
use objc2_core_graphics::CGColorSpace;
use objc2_core_image::{CIColor, CIContext, CIImage, kCIContextCacheIntermediates};
use objc2_core_video::*;
use objc2_foundation::{
    NSDictionary, NSNumber, NSObject, NSObjectProtocol, NSRunLoop, NSRunLoopCommonModes, NSTimer,
};
use objc2_metal::*;
use objc2_quartz_core::{
    CAMetalDisplayLink, CAMetalDisplayLinkDelegate, CAMetalDisplayLinkUpdate, CAMetalDrawable,
    CAMetalLayer,
};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::Rc;

const POOL_CAPACITY: i32 = 6;
const PREVIEW_IN_FLIGHT: usize = 2;

/// A completed, immutable, IOSurface-backed BGRA canvas, not a capture sample.
#[derive(Clone)]
pub struct RenderedFrame {
    buffer: CFRetained<CVPixelBuffer>,
    pub source_size: Resolution,
    pub canvas: Resolution,
    pub transform: ViewportTransform,
    pub timestamp_seconds: f64,
}

// SAFETY: Only Renderer constructs this concrete type, after its Metal command
// buffer has completed successfully. The privately owned CVPixelBuffer has
// system-owned IOSurface storage and no custom callbacks. No safe API exposes
// native aliases or writes. Clones only CF-retain the same immutable storage;
// the pool cannot recycle it while any clone/native reader retains it. CF
// retain/release is valid on arbitrary threads. This is not a blanket audit of
// CVPixelBuffer, CIContext, Metal objects, or arbitrary native holders.
unsafe impl Send for RenderedFrame {}
// SAFETY: Same completed-buffer invariant; safe shared reads expose only Rust
// metadata. Unsafe native readers must obey the explicit contract below.
unsafe impl Sync for RenderedFrame {}

impl RenderedFrame {
    /// Read access for presentation and the future writer, never for editing.
    ///
    /// # Safety
    /// Never mutate data, attachments, planes, or backing storage; never expose
    /// this reference/derived aliases through safe APIs. Keep a frame clone
    /// alive until every asynchronous native/GPU read completes, and release
    /// derived native aliases before that clone. Do not retain aliases beyond
    /// that read operation. CPU reads must use a read-only pixel-buffer lock.
    pub(crate) unsafe fn pixel_buffer_read(&self) -> &CVPixelBuffer {
        &self.buffer
    }
}

struct Gpu {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    context: Retained<CIContext>,
    color_space: CFRetained<CGColorSpace>,
    _local: PhantomData<Rc<()>>,
}

impl Gpu {
    fn new() -> Result<Self, String> {
        let device = MTLCreateSystemDefaultDevice().ok_or("Metal device unavailable")?;
        let queue = device
            .newCommandQueue()
            .ok_or("Cannot create Metal command queue")?;
        let cache = NSNumber::new_bool(false);
        // SAFETY: A correctly typed CI option; this context and queue are local
        // to one renderer/presenter. Disable cross-frame intermediate caching.
        let context = unsafe {
            let options = NSDictionary::from_slices(&[kCIContextCacheIntermediates], &[&*cache]);
            CIContext::contextWithMTLCommandQueue_options(&queue, Some(options.cast_unchecked()))
        };
        let color_space = CGColorSpace::new_device_rgb().ok_or("Cannot create RGB color space")?;
        Ok(Self {
            device,
            queue,
            context,
            color_space,
            _local: PhantomData,
        })
    }

    fn command(&self) -> Result<Retained<ProtocolObject<dyn MTLCommandBuffer>>, String> {
        self.queue
            .commandBuffer()
            .ok_or("Cannot create Metal command buffer".into())
    }

    // Caller keeps both the input image and destination alive to completion.
    unsafe fn encode(
        &self,
        image: &CIImage,
        texture: &ProtocolObject<dyn MTLTexture>,
        command: &ProtocolObject<dyn MTLCommandBuffer>,
        size: Resolution,
    ) {
        unsafe {
            // Store top-left rows for CV buffers and offscreen Metal textures.
            // Native drawables need a presentation-only correction (see update).
            let image = image.imageByApplyingTransform(CGAffineTransform {
                a: 1.0,
                b: 0.0,
                c: 0.0,
                d: -1.0,
                tx: 0.0,
                ty: f64::from(size.height),
            });
            self.context
                .render_toMTLTexture_commandBuffer_bounds_colorSpace(
                    &image,
                    texture,
                    Some(command),
                    bounds(size),
                    &self.color_space,
                );
        }
    }
}

struct Pool {
    native: CFRetained<CVPixelBufferPool>,
    threshold: CFRetained<CFDictionary<CFString, CFType>>,
    canvas: Resolution,
}

impl Pool {
    fn new(canvas: Resolution) -> Result<Self, String> {
        if canvas.width == 0
            || canvas.height == 0
            || canvas.width > 8192
            || canvas.height > 8192
            || u64::from(canvas.width) * u64::from(canvas.height) > 33_554_432
        {
            return Err("Canvas must be nonzero and at most 8192 per axis / 32 megapixels".into());
        }
        let width = CFNumber::new_i64(i64::from(canvas.width));
        let height = CFNumber::new_i64(i64::from(canvas.height));
        let format = CFNumber::new_i64(i64::from(kCVPixelFormatType_32BGRA));
        let surface = CFDictionary::<CFString, CFType>::empty();
        // SAFETY: Core Video's documented keys and value types. Empty IOSurface
        // properties requests system-owned storage, with no borrowed callbacks.
        unsafe {
            let attributes = CFDictionary::<CFString, CFType>::from_slices(
                &[
                    kCVPixelBufferWidthKey,
                    kCVPixelBufferHeightKey,
                    kCVPixelBufferPixelFormatTypeKey,
                    kCVPixelBufferMetalCompatibilityKey,
                    kCVPixelBufferIOSurfacePropertiesKey,
                ],
                &[
                    width.as_ref(),
                    height.as_ref(),
                    format.as_ref(),
                    CFBoolean::new(true).as_ref(),
                    surface.as_ref(),
                ],
            );
            let mut raw = std::ptr::null_mut();
            check_cv(
                CVPixelBufferPool::create(
                    None,
                    None,
                    Some(attributes.as_opaque()),
                    NonNull::from(&mut raw),
                ),
                "Create pixel buffer pool",
            )?;
            let native = CFRetained::from_raw(NonNull::new(raw).ok_or("Null pixel buffer pool")?);
            let limit = CFNumber::new_i32(POOL_CAPACITY);
            let threshold = CFDictionary::from_slices(
                &[kCVPixelBufferPoolAllocationThresholdKey],
                &[limit.as_ref()],
            );
            Ok(Self {
                native,
                threshold,
                canvas,
            })
        }
    }

    fn allocate(&self) -> Result<CFRetained<CVPixelBuffer>, String> {
        // SAFETY: Every allocation uses the threshold, not the unbounded API.
        unsafe {
            let mut raw = std::ptr::null_mut();
            check_cv(
                CVPixelBufferPool::create_pixel_buffer_with_aux_attributes(
                    None,
                    &self.native,
                    Some(self.threshold.as_opaque()),
                    NonNull::from(&mut raw),
                ),
                "Allocate bounded canvas buffer (held frames may exhaust the pool)",
            )?;
            Ok(CFRetained::from_raw(
                NonNull::new(raw).ok_or("Null canvas buffer")?,
            ))
        }
    }
}

/// Construct inside the recording render worker. One canvas/pool per renderer;
/// changing recording resolution requires a new renderer, not a cache of pools.
pub struct Renderer {
    gpu: Gpu,
    cache: CFRetained<CVMetalTextureCache>,
    pool: Option<Pool>,
}

impl Renderer {
    pub fn new() -> Result<Self, String> {
        autoreleasepool(|_| {
            let gpu = Gpu::new()?;
            let mut raw = std::ptr::null_mut();
            let cache = unsafe {
                check_cv(
                    CVMetalTextureCache::create(
                        None,
                        None,
                        &gpu.device,
                        None,
                        NonNull::from(&mut raw),
                    ),
                    "Create Metal texture cache",
                )?;
                CFRetained::from_raw(NonNull::new(raw).ok_or("Null Metal texture cache")?)
            };
            Ok(Self {
                gpu,
                cache,
                pool: None,
            })
        })
    }

    pub fn render(
        &mut self,
        sample: &NativeSample,
        transform: ViewportTransform,
        canvas: Resolution,
    ) -> Result<RenderedFrame, String> {
        autoreleasepool(|_| {
            let source_size = sample
                .dimensions()
                .ok_or("Sample has no video dimensions")?;
            let g = transform::geometry(transform, source_size, canvas)?;
            // SAFETY: Read only. Sample and image stay alive until GPU completion;
            // context intermediates are cleared before leaving this operation.
            unsafe {
                let buffer = sample
                    .image_buffer_read()
                    .ok_or("Sample has no image buffer")?;
                let image = CIImage::initWithCVPixelBuffer(CIImage::alloc(), &buffer);
                let image = compose(&image, g, source_size, canvas);
                let buffer = self.render_image(&image, canvas)?;
                Ok(RenderedFrame {
                    buffer,
                    source_size,
                    canvas,
                    transform,
                    timestamp_seconds: sample.timestamp_seconds(),
                })
            }
        })
    }

    fn render_image(
        &mut self,
        image: &CIImage,
        canvas: Resolution,
    ) -> Result<CFRetained<CVPixelBuffer>, String> {
        if let Some(pool) = &self.pool {
            if pool.canvas != canvas {
                return Err("Renderer canvas is fixed; create a new renderer for a new recording resolution".into());
            }
        } else {
            self.pool = Some(Pool::new(canvas)?);
        }
        let buffer = self.pool.as_ref().unwrap().allocate()?;
        // SAFETY: Destination is unpublished and exclusively owned here. All
        // texture aliases are dropped and cache flushed before publication.
        unsafe {
            let mut raw = std::ptr::null_mut();
            let usage = CFNumber::new_i64(
                (MTLTextureUsage::ShaderRead
                    | MTLTextureUsage::ShaderWrite
                    | MTLTextureUsage::RenderTarget)
                    .0 as i64,
            );
            let attrs = CFDictionary::from_slices(&[kCVMetalTextureUsage], &[&*usage]);
            check_cv(
                CVMetalTextureCache::create_texture_from_image(
                    None,
                    &self.cache,
                    &buffer,
                    Some(attrs.as_opaque()),
                    MTLPixelFormat::BGRA8Unorm,
                    canvas.width as usize,
                    canvas.height as usize,
                    0,
                    NonNull::from(&mut raw),
                ),
                "Create destination Metal texture",
            )?;
            let cv_texture =
                CFRetained::from_raw(NonNull::new(raw).ok_or("Null destination Metal texture")?);
            let texture =
                CVMetalTextureGetTexture(&cv_texture).ok_or("Missing destination Metal texture")?;
            let command = self.gpu.command()?;
            self.gpu.encode(image, &texture, &command, canvas);
            command.commit();
            command.waitUntilCompleted();
            let result = completed(&command);
            self.gpu.context.clearCaches();
            drop(command);
            drop(texture);
            drop(cv_texture);
            self.cache.flush(0);
            result?;
        }
        Ok(buffer)
    }
}

fn bounds(size: Resolution) -> CGRect {
    CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize {
            width: f64::from(size.width),
            height: f64::from(size.height),
        },
    }
}

// Convert the public top-left contract once to Core Image's bottom-left space.
unsafe fn compose(
    image: &CIImage,
    g: Geometry,
    source: Resolution,
    canvas: Resolution,
) -> Retained<CIImage> {
    unsafe {
        let crop_y = f64::from(source.height) - g.crop.y - g.crop.height;
        let crop = CGRect {
            origin: CGPoint {
                x: g.crop.x,
                y: crop_y,
            },
            size: CGSize {
                width: g.crop.width,
                height: g.crop.height,
            },
        };
        let sx = g.placement.width / g.crop.width;
        let sy = g.placement.height / g.crop.height;
        let matrix = CGAffineTransform {
            a: sx,
            b: 0.0,
            c: 0.0,
            d: sy,
            tx: g.placement.x - g.crop.x * sx,
            ty: f64::from(canvas.height) - g.placement.y - g.placement.height - crop_y * sy,
        };
        let foreground = image
            .imageByCroppingToRect(crop)
            .imageByApplyingTransform(matrix);
        let black = CIImage::initWithColor(CIImage::alloc(), &CIColor::blackColor())
            .imageByCroppingToRect(bounds(canvas));
        foreground
            .imageByCompositingOverImage(&black)
            .imageByCroppingToRect(bounds(canvas))
    }
}

fn check_cv(status: i32, operation: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("{operation}: Core Video status {status}"))
    }
}

fn completed(command: &ProtocolObject<dyn MTLCommandBuffer>) -> Result<(), String> {
    if command.status() == MTLCommandBufferStatus::Completed {
        Ok(())
    } else {
        Err(format!("Metal rendering failed: {:?}", command.error()))
    }
}

struct Presentation {
    command: Retained<ProtocolObject<dyn MTLCommandBuffer>>,
    _image: Retained<CIImage>,
    _texture: Retained<ProtocolObject<dyn MTLTexture>>,
    // Offscreen GPU tests have a texture but no window-system drawable.
    _drawable: Option<Retained<ProtocolObject<dyn CAMetalDrawable>>>,
    _frame: RenderedFrame,
}

struct Retirement {
    timer: Retained<NSTimer>,
    _layer: Retained<CAMetalLayer>,
    // Explicit self-retain, independent of the run loop retaining its timer.
    // If the app stops its run loop, keep unfinished GPU reads alive through
    // process exit, even if Foundation invalidates/releases the timer. Normal
    // polling breaks this cycle only after every command reaches a terminal state.
    _delegate: Retained<PreviewDelegate>,
}

struct PreviewState {
    gpu: Gpu,
    pending: VecDeque<Presentation>,
    latest: Option<RenderedFrame>,
    error: Option<String>,
    presented_frames: u64,
    retirement: Option<Retirement>,
}

impl PreviewState {
    fn poll_completed(&mut self) {
        while let Some(front) = self.pending.front() {
            let status = front.command.status();
            if status != MTLCommandBufferStatus::Completed
                && status != MTLCommandBufferStatus::Error
            {
                break;
            }
            let finished = self.pending.pop_front().unwrap();
            match completed(&finished.command) {
                Ok(()) => self.presented_frames = self.presented_frames.saturating_add(1),
                Err(error) => self.error = Some(error),
            }
        }
    }

    fn update(&mut self, update: &CAMetalDisplayLinkUpdate) -> Result<(), String> {
        self.poll_completed();
        if self.error.is_some() {
            return Ok(());
        }
        if self.pending.len() >= PREVIEW_IN_FLIGHT {
            return Ok(());
        }
        let Some(frame) = self.latest.take() else {
            return Ok(());
        };
        // The display link supplies an available drawable. Never call
        // nextDrawable on the UI thread: it can block even with a timeout.
        let drawable = update.drawable();
        let texture = drawable.texture();
        let size = Resolution {
            width: u32::try_from(texture.width()).map_err(|_| "Preview texture too wide")?,
            height: u32::try_from(texture.height()).map_err(|_| "Preview texture too tall")?,
        };
        // SAFETY: This reads the already rendered surface. Pending retains
        // the image, drawable and frame until the submitted command completes.
        unsafe {
            let image = presentation_image(&frame, size)?;
            // CAMetalLayer's composited drawable is vertically reversed relative
            // to the offscreen texture path. Compensate only here, not in the
            // shared rendered buffer used by the future writer. NSView/layer
            // geometry flipping does not change these drawable contents.
            let image = image.imageByApplyingTransform(CGAffineTransform {
                a: 1.0,
                b: 0.0,
                c: 0.0,
                d: -1.0,
                tx: 0.0,
                ty: f64::from(size.height),
            });
            let command = self.gpu.command()?;
            self.gpu.encode(&image, &texture, &command, size);
            command.presentDrawable(ProtocolObject::from_ref(&*drawable));
            command.commit();
            self.pending.push_back(Presentation {
                command,
                _image: image,
                _texture: texture,
                _drawable: Some(drawable),
                _frame: frame,
            });
        }
        Ok(())
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "OculiMetalPreviewDelegate"]
    #[thread_kind = MainThreadOnly]
    #[ivars = RefCell<PreviewState>]
    struct PreviewDelegate;

    unsafe impl NSObjectProtocol for PreviewDelegate {}

    impl PreviewDelegate {
        #[unsafe(method(retirePreview:))]
        fn retire_preview(&self, timer: &NSTimer) {
            // Keep self alive while breaking the explicit retirement cycle.
            let _keep_alive = unsafe { Retained::retain(std::ptr::from_ref(self).cast_mut()) };
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                autoreleasepool(|_| {
                    let Ok(mut state) = self.ivars().try_borrow_mut() else { return; };
                    state.poll_completed();
                    if state.pending.is_empty() {
                        unsafe { state.gpu.context.clearCaches(); }
                        timer.invalidate();
                        if let Some(retirement) = state.retirement.take() {
                            retirement.timer.invalidate();
                        }
                    }
                });
            }));
        }
    }

    unsafe impl CAMetalDisplayLinkDelegate for PreviewDelegate {
        #[unsafe(method(metalDisplayLink:needsUpdate:))]
        fn needs_update(&self, link: &CAMetalDisplayLink, update: &CAMetalDisplayLinkUpdate) {
            // Registered on the main run loop only. Never unwind through ObjC.
            let Ok(mut state) = self.ivars().try_borrow_mut() else { return; };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                autoreleasepool(|_| state.update(update))
            }));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => state.error = Some(error),
                Err(_) => state.error = Some("Preview callback panicked".into()),
            }
            if state.error.is_some() || (state.latest.is_none() && state.pending.is_empty()) {
                link.setPaused(true);
            }
        }
    }
);

/// Main-thread-owned native preview (macOS 14+). `present` replaces one pending
/// frame without waiting for a drawable or GPU. The main-run-loop display link
/// submits at most two GPU commands; preview never recomposes the capture source.
/// Drop retires those submissions asynchronously on the main run loop. If that
/// loop stops at app exit, unfinished resources remain retained until process exit.
pub struct PreviewPresenter {
    layer: Retained<CAMetalLayer>,
    link: Retained<CAMetalDisplayLink>,
    delegate: Retained<PreviewDelegate>,
    _main_thread: MainThreadMarker,
}

impl PreviewPresenter {
    pub fn new() -> Result<Self, String> {
        let main_thread =
            MainThreadMarker::new().ok_or("PreviewPresenter must be created on the main thread")?;
        if objc2::runtime::AnyClass::get(c"CAMetalDisplayLink").is_none() {
            return Err("Nonblocking Metal preview requires macOS 14 or later".into());
        }
        autoreleasepool(|_| {
            let gpu = Gpu::new()?;
            let layer = CAMetalLayer::new();
            layer.setDevice(Some(&gpu.device));
            layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
            layer.setFramebufferOnly(false);
            layer.setMaximumDrawableCount(3);
            layer.setAllowsNextDrawableTimeout(true);
            layer.setOpaque(true);
            let allocated =
                PreviewDelegate::alloc(main_thread).set_ivars(RefCell::new(PreviewState {
                    gpu,
                    pending: VecDeque::with_capacity(PREVIEW_IN_FLIGHT),
                    latest: None,
                    error: None,
                    presented_frames: 0,
                    retirement: None,
                }));
            // SAFETY: Initialize our main-thread NSObject subclass once.
            let delegate: Retained<PreviewDelegate> = unsafe { msg_send![super(allocated), init] };
            let link = CAMetalDisplayLink::initWithMetalLayer(CAMetalDisplayLink::alloc(), &layer);
            link.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            link.setPaused(true);
            // SAFETY: Presenter creation requires the main thread, and the
            // delegate/state are exclusively accessed by that same run loop.
            unsafe {
                link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes);
            }
            Ok(Self {
                layer,
                link,
                delegate,
                _main_thread: main_thread,
            })
        })
    }

    pub fn layer(&self) -> &CAMetalLayer {
        &self.layer
    }

    /// Successfully completed display-link submissions (not merely queued frames).
    /// Polls command status without waiting; completion does not prove scanout.
    pub fn presented_frames(&self) -> u64 {
        let mut state = self.delegate.ivars().borrow_mut();
        state.poll_completed();
        state.presented_frames
    }

    pub fn present(
        &mut self,
        frame: &RenderedFrame,
        width_points: f64,
        height_points: f64,
        backing_scale: f64,
    ) -> Result<(), String> {
        autoreleasepool(|_| {
            let size = transform::backing_size(width_points, height_points, backing_scale)?;
            let mut state = self.delegate.ivars().borrow_mut();
            if let Some(error) = state.error.take() {
                return Err(error);
            }
            self.layer.setContentsScale(backing_scale);
            self.layer.setDrawableSize(bounds(size).size);
            state.latest = Some(frame.clone());
            self.link.setPaused(false);
            Ok(())
        })
    }
}

impl Drop for PreviewPresenter {
    fn drop(&mut self) {
        self.link.invalidate();
        self.link.setDelegate(None);
        let mut state = self.delegate.ivars().borrow_mut();
        state.latest = None;
        state.poll_completed();
        if state.pending.is_empty() {
            unsafe {
                state.gpu.context.clearCaches();
            }
            return;
        }
        // SAFETY: The selector has NSTimer's required signature. Only the main
        // run loop accesses this main-thread delegate. Retirement owns the exact
        // GPU/context, layer, images, textures, drawables and immutable frames;
        // no native retention assumptions or cross-thread native wrappers are
        // needed. One timer retires at most PREVIEW_IN_FLIGHT existing commands
        // and never submits more. The self-retain also survives run-loop shutdown.
        unsafe {
            let timer = NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
                1.0 / 60.0,
                &self.delegate,
                sel!(retirePreview:),
                None,
                true,
            );
            state.retirement = Some(Retirement {
                timer: timer.clone(),
                _layer: self.layer.clone(),
                _delegate: self.delegate.clone(),
            });
            NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
        }
    }
}

unsafe fn presentation_image(
    frame: &RenderedFrame,
    size: Resolution,
) -> Result<Retained<CIImage>, String> {
    unsafe {
        let buffer = frame.pixel_buffer_read();
        let canvas = Resolution {
            width: CVPixelBufferGetWidth(buffer) as u32,
            height: CVPixelBufferGetHeight(buffer) as u32,
        };
        let image = CIImage::initWithCVPixelBuffer(CIImage::alloc(), buffer);
        // Scaling the fixed canvas for the display only; never reapply its crop,
        // source placement or source transform. Preserve black letterboxing.
        Ok(compose(
            &image,
            transform::geometry(ViewportTransform::default(), canvas, size)?,
            canvas,
            size,
        ))
    }
}

/// Permission-independent smoke input: four color landmarks in a fitted canvas.
/// Can run on a worker; only attaching/presenting its result requires the UI.
pub fn synthetic_preview_frame() -> Result<RenderedFrame, String> {
    autoreleasepool(|_| {
        let sample = super::synthetic_sample(80, 40)?;
        Renderer::new()?.render(
            &sample,
            ViewportTransform::default(),
            Resolution {
                width: 160,
                height: 120,
            },
        )
    })
}

pub(crate) fn synthetic_sample(width: u32, height: u32) -> Result<NativeSample, String> {
    use crate::platform::macos::sample::SampleKind;
    use objc2_core_media::{
        CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags,
        CMVideoFormatDescriptionCreateForImageBuffer,
    };
    // SAFETY: Private system-owned buffer, written only before wrapping it in
    // an immutable ready sample. No raw aliases escape this construction.
    unsafe {
        let pool = Pool::new(Resolution { width, height })?;
        let buffer = pool.allocate()?;
        check_cv(
            CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()),
            "Lock synthetic buffer",
        )?;
        let base = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
        let stride = CVPixelBufferGetBytesPerRow(&buffer);
        for y in 0..height as usize {
            for x in 0..width as usize {
                let bgra = match (x < width as usize / 2, y < height as usize / 2) {
                    (true, true) => [0, 0, 255, 255],
                    (false, true) => [0, 255, 0, 255],
                    (true, false) => [255, 0, 0, 255],
                    (false, false) => [255, 255, 255, 255],
                };
                std::ptr::copy_nonoverlapping(bgra.as_ptr(), base.add(y * stride + x * 4), 4);
            }
        }
        check_cv(
            CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()),
            "Unlock synthetic buffer",
        )?;
        let mut format_raw = std::ptr::null();
        check_cv(
            CMVideoFormatDescriptionCreateForImageBuffer(
                None,
                &buffer,
                NonNull::from(&mut format_raw),
            ),
            "Create synthetic format",
        )?;
        let format = CFRetained::from_raw(
            NonNull::new(format_raw.cast_mut()).ok_or("Null synthetic format")?,
        );
        let time = CMTime {
            value: 15,
            timescale: 30,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        let timing = CMSampleTimingInfo {
            duration: CMTime { value: 1, ..time },
            presentationTimeStamp: time,
            decodeTimeStamp: time,
        };
        let mut sample_raw = std::ptr::null_mut();
        check_cv(
            CMSampleBuffer::create_ready_with_image_buffer(
                None,
                &buffer,
                &format,
                NonNull::from(&timing),
                NonNull::from(&mut sample_raw),
            ),
            "Create synthetic sample",
        )?;
        let sample = CFRetained::from_raw(NonNull::new(sample_raw).ok_or("Null synthetic sample")?);
        NativeSample::retain(&sample, SampleKind::Video).ok_or("Synthetic sample not ready".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(width: u32, height: u32) -> Resolution {
        Resolution { width, height }
    }

    fn pixel(frame: &RenderedFrame, x: usize, y: usize) -> [u8; 4] {
        // SAFETY: GPU has completed; only read under a read-only lock. No
        // pointer escapes, and the frame is retained for the entire read.
        unsafe {
            let buffer = frame.pixel_buffer_read();
            assert_eq!(
                CVPixelBufferGetPixelFormatType(buffer),
                kCVPixelFormatType_32BGRA
            );
            assert_eq!(CVPixelBufferGetWidth(buffer), frame.canvas.width as usize);
            assert_eq!(CVPixelBufferGetHeight(buffer), frame.canvas.height as usize);
            assert_eq!(
                CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly),
                0
            );
            let address = CVPixelBufferGetBaseAddress(buffer).cast::<u8>();
            let stride = CVPixelBufferGetBytesPerRow(buffer);
            let result =
                std::ptr::read_unaligned(address.add(y * stride + x * 4).cast::<[u8; 4]>());
            assert_eq!(
                CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly),
                0
            );
            result
        }
    }

    #[test]
    fn completed_frame_is_send_sync_clone() {
        fn check<T: Send + Sync + Clone>() {}
        check::<RenderedFrame>();
    }

    #[test]
    fn preview_teardown_never_waits_for_gpu() {
        let source = include_str!("macos.rs");
        let teardown = source
            .split("impl Drop for PreviewPresenter {")
            .nth(1)
            .unwrap()
            .split("unsafe fn presentation_image(")
            .next()
            .unwrap();
        assert!(!teardown.contains("waitUntilCompleted"));
    }

    #[test]
    fn preview_poll_retains_unfinished_surfaces_and_counts_completion_once() {
        if MTLCreateSystemDefaultDevice().is_none() {
            eprintln!(
                "SKIP preview_poll_retains_unfinished_surfaces_and_counts_completion_once: no Metal device"
            );
            return;
        }
        autoreleasepool(|_| {
            let sample = super::super::synthetic_sample(80, 40).unwrap();
            let canvas = size(80, 80);
            let mut renderer = Renderer::new().unwrap();
            let mut state = PreviewState {
                gpu: Gpu::new().unwrap(),
                pending: VecDeque::new(),
                latest: None,
                error: None,
                presented_frames: 0,
                retirement: None,
            };
            let _held = autoreleasepool(|_| {
                for _ in 0..PREVIEW_IN_FLIGHT {
                    let frame = renderer
                        .render(&sample, Default::default(), canvas)
                        .unwrap();
                    let descriptor = MTLTextureDescriptor::new();
                    // SAFETY: Nonzero dimensions within the device's texture limits.
                    unsafe {
                        descriptor.setWidth(canvas.width as usize);
                        descriptor.setHeight(canvas.height as usize);
                    }
                    descriptor.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
                    descriptor.setUsage(
                        MTLTextureUsage::ShaderRead
                            | MTLTextureUsage::ShaderWrite
                            | MTLTextureUsage::RenderTarget,
                    );
                    let texture = state
                        .gpu
                        .device
                        .newTextureWithDescriptor(&descriptor)
                        .unwrap();
                    let image = unsafe { presentation_image(&frame, canvas).unwrap() };
                    let command = state.gpu.command().unwrap();
                    unsafe {
                        state.gpu.encode(&image, &texture, &command, canvas);
                    }
                    // Deliberately defer commit: polling must return immediately
                    // and must not recycle storage for a nonterminal command.
                    state.pending.push_back(Presentation {
                        command,
                        _image: image,
                        _texture: texture,
                        _drawable: None,
                        _frame: frame,
                    });
                }
                let held: Vec<_> = (PREVIEW_IN_FLIGHT..POOL_CAPACITY as usize)
                    .map(|_| {
                        renderer
                            .render(&sample, Default::default(), canvas)
                            .unwrap()
                    })
                    .collect();
                state.poll_completed();
                assert_eq!(state.pending.len(), PREVIEW_IN_FLIGHT);
                assert_eq!(state.presented_frames, 0);
                assert!(
                    renderer
                        .render(&sample, Default::default(), canvas)
                        .is_err()
                );
                for pending in &state.pending {
                    pending.command.commit();
                }
                // This test runs off the AppKit thread. Only the test synchronizes;
                // production polling and presenter teardown never wait for the GPU.
                for pending in &state.pending {
                    pending.command.waitUntilCompleted();
                    completed(&pending.command).unwrap();
                }
                state.poll_completed();
                assert!(state.pending.is_empty());
                assert!(state.error.is_none());
                assert_eq!(state.presented_frames, PREVIEW_IN_FLIGHT as u64);
                state.poll_completed();
                assert_eq!(state.presented_frames, PREVIEW_IN_FLIGHT as u64);
                unsafe {
                    state.gpu.context.clearCaches();
                }
                held
            });
            assert!(renderer.render(&sample, Default::default(), canvas).is_ok());
        });
    }

    #[test]
    fn gpu_landmarks_pool_bound_and_same_surface_preview() {
        if MTLCreateSystemDefaultDevice().is_none() {
            eprintln!("SKIP gpu_landmarks_pool_bound_and_same_surface_preview: no Metal device");
            return;
        }
        autoreleasepool(|_| {
            let sample = synthetic_sample(80, 40).unwrap();
            assert_eq!(sample.dimensions(), Some(size(80, 40)));
            let canvas = size(80, 80);
            let mut renderer = Renderer::new().unwrap();
            let frame = renderer
                .render(&sample, Default::default(), canvas)
                .unwrap();
            assert_eq!(frame.source_size, size(80, 40));
            assert_eq!(frame.timestamp_seconds, 0.5);
            assert_eq!(pixel(&frame, 10, 5), [0, 0, 0, 255]);
            assert_eq!(pixel(&frame, 10, 30), [0, 0, 255, 255]);
            assert_eq!(pixel(&frame, 70, 30), [0, 255, 0, 255]);
            assert_eq!(pixel(&frame, 10, 50), [255, 0, 0, 255]);
            assert_eq!(pixel(&frame, 70, 50), [255, 255, 255, 255]);
            let crop = ViewportTransform {
                crop_width: 0.5,
                crop_height: 0.5,
                ..Default::default()
            };
            let cropped = renderer.render(&sample, crop, canvas).unwrap();
            assert_eq!(pixel(&cropped, 40, 40), [0, 0, 255, 255]);
            let moved = renderer
                .render(
                    &sample,
                    ViewportTransform {
                        offset_y: 0.25,
                        offset_x: 0.25,
                        ..Default::default()
                    },
                    canvas,
                )
                .unwrap();
            assert_eq!(pixel(&moved, 10, 50), [0, 0, 0, 255]);
            assert_eq!(pixel(&moved, 30, 30), [0, 0, 0, 255]);
            assert_eq!(pixel(&moved, 30, 50), [0, 0, 255, 255]);

            // The future writer and presenter borrow exactly this retained
            // surface. Exercise the presenter's actual image-building path
            // offscreen, without creating NSApplication, NSView or CAMetalLayer.
            let writer_frame = moved.clone();
            unsafe {
                assert!(std::ptr::eq(
                    writer_frame.pixel_buffer_read(),
                    moved.pixel_buffer_read()
                ));
            }
            let preview_canvas = size(160, 80);
            let mut preview_renderer = Renderer::new().unwrap();
            let preview_buffer = autoreleasepool(|_| unsafe {
                let image = presentation_image(&moved, preview_canvas).unwrap();
                preview_renderer
                    .render_image(&image, preview_canvas)
                    .unwrap()
            });
            let preview = RenderedFrame {
                buffer: preview_buffer,
                canvas: preview_canvas,
                ..moved.clone()
            };
            assert_eq!(pixel(&preview, 10, 50), [0, 0, 0, 255]);
            assert_eq!(pixel(&preview, 70, 50), pixel(&writer_frame, 30, 50));
            assert_eq!(pixel(&preview, 70, 30), pixel(&writer_frame, 30, 30));

            let resized = synthetic_sample(160, 80).unwrap();
            let resized_frame = renderer.render(&resized, crop, canvas).unwrap();
            assert_eq!(pixel(&resized_frame, 40, 40), pixel(&cropped, 40, 40));
            assert!(
                renderer
                    .render(&sample, Default::default(), size(100, 100))
                    .is_err()
            );
            drop(resized_frame);
            drop(cropped);
            drop(moved);
            drop(writer_frame);
            drop(preview);
            let mut held = vec![frame];
            for _ in 1..POOL_CAPACITY {
                held.push(
                    renderer
                        .render(&sample, Default::default(), canvas)
                        .unwrap(),
                );
            }
            assert!(
                renderer
                    .render(&sample, Default::default(), canvas)
                    .is_err()
            );
            held.pop();
            assert!(renderer.render(&sample, Default::default(), canvas).is_ok());
        });
    }
}
