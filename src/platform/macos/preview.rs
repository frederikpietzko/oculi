//! Latest-only handoffs isolate capture/audio draining from GPU rendering.

use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;

use super::sample::NativeSample;
use crate::capture::SourceKey;
use crate::model::{RecordingId, Resolution, ViewportTransform};
use crate::render::{RenderedFrame, Renderer};

#[derive(Clone, Debug, PartialEq)]
pub struct PreviewConfig {
    pub recording: RecordingId,
    pub source: SourceKey,
    pub transform: ViewportTransform,
    pub canvas: Resolution,
    pub fps: u32,
    pub active: bool,
    pub visible: bool,
}

impl PreviewConfig {
    fn interval(&self) -> Option<Duration> {
        self.visible.then(|| {
            Duration::from_secs_f64(
                1.0 / f64::from(self.fps.clamp(1, if self.active { 30 } else { 5 })),
            )
        })
    }
}

#[derive(Clone)]
pub(super) struct PreviewInput {
    pub source: SourceKey,
    pub sample: NativeSample,
    pub revision: u64,
}

struct Output {
    config: PreviewConfig,
    result: Result<RenderedFrame, String>,
}

#[derive(Clone)]
pub(crate) struct PreviewBridge {
    pub configs: Arc<ArcSwap<Vec<PreviewConfig>>>,
    pub(super) inputs: Arc<ArcSwap<HashMap<RecordingId, PreviewInput>>>,
    recording: Arc<ArcSwap<Vec<PreviewConfig>>>,
    outputs: Arc<ArcSwap<HashMap<RecordingId, Output>>>,
    finished: Arc<AtomicBool>,
}

struct Pipeline {
    renderer: Result<Renderer, String>,
    last_render: Option<Instant>,
    revision: Option<u64>,
    config: PreviewConfig,
}

impl PreviewBridge {
    pub fn new(stop: Arc<AtomicBool>) -> Self {
        let bridge = Self {
            configs: Arc::new(ArcSwap::from_pointee(Vec::new())),
            inputs: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            recording: Arc::new(ArcSwap::from_pointee(Vec::new())),
            outputs: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            finished: Arc::new(AtomicBool::new(false)),
        };
        let worker = bridge.clone();
        std::thread::spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run(&stop)));
            if result.is_err() {
                let outputs = worker
                    .configs
                    .load()
                    .iter()
                    .map(|config| {
                        (
                            config.recording,
                            Output {
                                config: config.clone(),
                                result: Err(
                                    "Preview worker stopped unexpectedly. Restart Oculi.".into()
                                ),
                            },
                        )
                    })
                    .collect();
                worker.outputs.store(Arc::new(outputs));
            }
            worker.finished.store(true, Ordering::Release);
        });
        bridge
    }

    pub fn configure(&self, configs: Vec<PreviewConfig>) {
        if **self.configs.load() != configs {
            self.configs.store(Arc::new(configs));
        }
    }

    pub fn frame(&self, recording: RecordingId) -> Option<Result<RenderedFrame, String>> {
        let configs = self.effective_configs();
        let config = configs
            .iter()
            .find(|config| config.recording == recording)?;
        let outputs = self.outputs.load();
        let output = outputs.get(&recording)?;
        // A changed source or canvas must never display an obsolete surface.
        if !config.visible
            || output.config.source != config.source
            || output.config.canvas != config.canvas
        {
            return None;
        }
        let inputs = self.inputs.load();
        if inputs
            .get(&recording)
            .is_none_or(|input| input.source != config.source)
        {
            return None;
        }
        Some(output.result.clone())
    }

    /// Locks source/output while allowing live UI transform edits. The render
    /// worker and recorder read the same immutable output, never a second render.
    pub(crate) fn record(&self, configs: Vec<PreviewConfig>) {
        self.recording.store(Arc::new(configs));
    }

    fn effective_configs(&self) -> Vec<PreviewConfig> {
        let ui = self.configs.load();
        let locked = self.recording.load();
        let mut configs = (**ui).clone();
        for required in locked.iter() {
            let mut config = required.clone();
            if let Some(edit) = configs
                .iter_mut()
                .find(|c| c.recording == required.recording)
            {
                if edit.source == required.source && edit.canvas == required.canvas {
                    config.transform = edit.transform;
                }
                config.visible = edit.visible;
                *edit = config;
            } else {
                config.visible = false;
                configs.push(config);
            }
        }
        configs
    }

    pub(crate) fn recording_frame(
        &self,
        recording: RecordingId,
    ) -> Option<Result<RenderedFrame, String>> {
        let configs = self.effective_configs();
        let config = configs
            .iter()
            .find(|config| config.recording == recording)?;
        let inputs = self.inputs.load();
        if inputs
            .get(&recording)
            .is_none_or(|input| input.source != config.source)
        {
            return None;
        }
        let outputs = self.outputs.load();
        let output = outputs.get(&recording)?;
        if output.config.source != config.source
            || output.config.canvas != config.canvas
            || output.config.transform != config.transform
        {
            return None;
        }
        Some(output.result.clone())
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn synthetic_input(
        &self,
        recording: RecordingId,
        source: SourceKey,
        sample: NativeSample,
    ) {
        let mut inputs = (**self.inputs.load()).clone();
        let revision = inputs.get(&recording).map_or(1, |input| input.revision + 1);
        inputs.insert(
            recording,
            PreviewInput {
                source,
                sample,
                revision,
            },
        );
        self.inputs.store(Arc::new(inputs));
    }

    fn run(&self, stop: &AtomicBool) {
        let mut pipelines = HashMap::<RecordingId, Pipeline>::new();
        while !stop.load(Ordering::Acquire) {
            objc2::rc::autoreleasepool(|_| {
                let configs = self.effective_configs();
                let recording = self.recording.load();
                let inputs = self.inputs.load_full();
                pipelines.retain(|id, _| configs.iter().any(|config| config.recording == *id));
                let previous = self.outputs.load_full();
                let mut outputs = HashMap::new();
                for config in configs.iter() {
                    let Some(input) = inputs
                        .get(&config.recording)
                        .filter(|input| input.source == config.source)
                    else {
                        pipelines.remove(&config.recording);
                        continue;
                    };
                    let interval = if recording.iter().any(|c| c.recording == config.recording) {
                        Some(Duration::from_secs_f64(
                            1.0 / f64::from(config.fps.clamp(1, 120)),
                        ))
                    } else {
                        config.interval()
                    };
                    let Some(interval) = interval else {
                        // Offscreen cards retain no GPU surface or pool.
                        pipelines.remove(&config.recording);
                        continue;
                    };
                    let pipeline = pipelines
                        .entry(config.recording)
                        .or_insert_with(|| Pipeline {
                            renderer: Renderer::new(),
                            last_render: None,
                            revision: None,
                            config: config.clone(),
                        });
                    let changed = pipeline.config.source != config.source
                        || pipeline.config.canvas != config.canvas
                        || pipeline.config.transform != config.transform;
                    let due = pipeline
                        .last_render
                        .is_none_or(|time| time.elapsed() >= interval);
                    let retry = previous
                        .get(&config.recording)
                        .is_some_and(|output| output.result.is_err());
                    let result =
                        if due && (changed || pipeline.revision != Some(input.revision) || retry) {
                            if pipeline.config.canvas != config.canvas {
                                pipeline.renderer = Renderer::new();
                            }
                            pipeline.last_render = Some(Instant::now());
                            pipeline.revision = Some(input.revision);
                            pipeline.config = config.clone();
                            match &mut pipeline.renderer {
                                Ok(renderer) => {
                                    renderer.render(&input.sample, config.transform, config.canvas)
                                }
                                Err(error) => Err(error.clone()),
                            }
                        } else if let Some(output) = previous.get(&config.recording) {
                            outputs.insert(
                                config.recording,
                                Output {
                                    config: output.config.clone(),
                                    result: output.result.clone(),
                                },
                            );
                            continue;
                        } else {
                            continue;
                        };
                    outputs.insert(
                        config.recording,
                        Output {
                            config: config.clone(),
                            result,
                        },
                    );
                }
                self.outputs.store(Arc::new(outputs));
            });
            std::thread::sleep(Duration::from_millis(5));
        }
        self.outputs.store(Arc::new(HashMap::new()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> PreviewConfig {
        PreviewConfig {
            recording: RecordingId::new(),
            source: SourceKey::Display(1),
            transform: ViewportTransform::default(),
            canvas: Resolution::default(),
            fps: 60,
            active: true,
            visible: true,
        }
    }

    #[test]
    fn inactive_and_hidden_previews_are_throttled() {
        let mut config = config();
        assert_eq!(config.interval(), Some(Duration::from_secs_f64(1.0 / 30.0)));
        config.active = false;
        assert_eq!(config.interval(), Some(Duration::from_millis(200)));
        config.visible = false;
        assert_eq!(config.interval(), None);
    }

    #[test]
    fn synthetic_worker_reframes_static_samples_and_clears_unavailable_sources() {
        let sample = match crate::render::synthetic_sample(80, 40) {
            Ok(sample) => sample,
            Err(error) if error == "Metal device unavailable" => {
                eprintln!("SKIP synthetic preview worker: {error}; NOT verified");
                return;
            }
            Err(error) => panic!("Synthetic preview input: {error}"),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let bridge = PreviewBridge::new(stop.clone());
        let mut a = config();
        a.canvas = Resolution {
            width: 160,
            height: 120,
        };
        let mut b = a.clone();
        b.recording = RecordingId::new();
        let inputs = [a.clone(), b.clone()]
            .into_iter()
            .map(|config| {
                (
                    config.recording,
                    PreviewInput {
                        source: config.source,
                        sample: sample.clone(),
                        revision: 1,
                    },
                )
            })
            .collect();
        bridge.inputs.store(Arc::new(inputs));
        bridge.configure(vec![a.clone(), b.clone()]);
        let wait = |config: &PreviewConfig| {
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                if let Some(Ok(frame)) = bridge.frame(config.recording)
                    && frame.transform == config.transform
                    && frame.canvas == config.canvas
                {
                    return frame;
                }
                assert!(
                    Instant::now() < deadline,
                    "Preview transform/canvas timeout"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let first = wait(&a);
        wait(&b);
        b.transform.scale = 2.0;
        b.transform.offset_x = 0.2;
        bridge.configure(vec![a.clone(), b.clone()]);
        let reframed = wait(&b);
        assert_eq!(
            reframed.timestamp_seconds, first.timestamp_seconds,
            "Static samples must reframe without a new capture timestamp"
        );
        assert_eq!(wait(&a).transform, first.transform);
        b.canvas = Resolution {
            width: 100,
            height: 80,
        };
        bridge.configure(vec![a.clone(), b.clone()]);
        wait(&b);
        assert_eq!(wait(&a).canvas, first.canvas);
        let mut invalid = b.clone();
        invalid.transform.scale = -1.0;
        bridge.configure(vec![a.clone(), invalid]);
        let deadline = Instant::now() + Duration::from_secs(4);
        while !matches!(bridge.frame(b.recording), Some(Err(_))) {
            assert!(
                Instant::now() < deadline,
                "Invalid transform was not reported"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        bridge.configure(vec![a.clone(), b.clone()]);
        wait(&b);
        let mut resized = (**bridge.inputs.load()).clone();
        resized.get_mut(&b.recording).unwrap().sample =
            crate::render::synthetic_sample(120, 80).unwrap();
        resized.get_mut(&b.recording).unwrap().revision = 2;
        bridge.inputs.store(Arc::new(resized));
        let deadline = Instant::now() + Duration::from_secs(4);
        while wait(&b).source_size
            != (Resolution {
                width: 120,
                height: 80,
            })
        {
            assert!(
                Instant::now() < deadline,
                "Source dimensions did not update"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        b.visible = false;
        bridge.configure(vec![a.clone(), b.clone()]);
        assert!(bridge.frame(b.recording).is_none());
        a.source = SourceKey::Display(2);
        bridge.configure(vec![a.clone(), b]);
        assert!(
            bridge.frame(a.recording).is_none(),
            "Changed sources must hide old surfaces immediately"
        );
        bridge.inputs.store(Arc::new(HashMap::new()));
        assert!(bridge.frame(a.recording).is_none());
        bridge.configure(vec![]);
        stop.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(4);
        while !bridge.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(bridge.is_finished());
        assert!(bridge.outputs.load().is_empty());
    }

    #[test]
    fn preview_configuration_is_coalesced_without_capture_commands() {
        let stop = Arc::new(AtomicBool::new(false));
        let bridge = PreviewBridge::new(stop.clone());
        let mut config = config();
        bridge.configure(vec![config.clone()]);
        let initial = bridge.configs.load_full();
        bridge.configure(vec![config.clone()]);
        assert!(Arc::ptr_eq(&initial, &bridge.configs.load_full()));
        config.transform.scale = 2.0;
        bridge.configure(vec![config]);
        assert!(!Arc::ptr_eq(&initial, &bridge.configs.load_full()));
        assert!(bridge.frame(initial[0].recording).is_none());
        stop.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !bridge.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(bridge.is_finished());
    }

    #[test]
    fn hidden_recordings_render_locked_outputs_and_share_preview_buffers() {
        let stop = Arc::new(AtomicBool::new(false));
        let bridge = PreviewBridge::new(stop.clone());
        let mut config = config();
        config.canvas = Resolution {
            width: 160,
            height: 80,
        };
        config.visible = false;
        config.active = false;
        let sample = crate::render::synthetic_sample(80, 40).unwrap();
        bridge.synthetic_input(config.recording, config.source.clone(), sample);
        bridge.configure(vec![config.clone()]);
        bridge.record(vec![config.clone()]);
        let wait = |expected: &PreviewConfig| {
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                if let Some(Ok(frame)) = bridge.recording_frame(expected.recording)
                    && frame.transform == expected.transform
                    && frame.canvas == expected.canvas
                {
                    return frame;
                }
                assert!(Instant::now() < deadline, "Hidden recording render timeout");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let hidden = wait(&config);
        assert!(bridge.frame(config.recording).is_none());
        config.visible = true;
        bridge.configure(vec![config.clone()]);
        let preview = bridge.frame(config.recording).unwrap().unwrap();
        // SAFETY: Compare identity only while both completed immutable frames
        // remain alive. No native alias or read escapes this test operation.
        unsafe {
            assert!(std::ptr::eq(
                hidden.pixel_buffer_read(),
                preview.pixel_buffer_read()
            ));
        }
        config.transform.crop_width = 0.5;
        bridge.configure(vec![config.clone()]);
        assert!(
            bridge.recording_frame(config.recording).is_none(),
            "Never encode the previous transform after an edit"
        );
        let edited = wait(&config);
        assert_eq!(edited.timestamp_seconds, hidden.timestamp_seconds);
        let mut incompatible = config.clone();
        incompatible.canvas.width = 320;
        incompatible.source = SourceKey::Display(99);
        bridge.configure(vec![incompatible]);
        let locked = bridge.effective_configs().pop().unwrap();
        assert_eq!(locked.canvas, config.canvas);
        assert_eq!(locked.source, config.source);
        bridge.record(Vec::new());
        stop.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(4);
        while !bridge.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(bridge.is_finished());
    }
}
