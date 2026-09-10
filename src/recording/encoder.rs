//! One native writer per worker. No filesystem/encoder waits on capture or UI.

use super::{
    queue::{MediaQueue, QueueStats},
    writer::Writer,
};
use crate::{
    model::OutputSettings, platform::macos::sample::NativeSample, render::RenderedFrame,
    storage::reservation::OutputReservation,
};
use arc_swap::ArcSwap;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

struct Video {
    frame: RenderedFrame,
    timestamp: f64,
}
struct Audio {
    sample: NativeSample,
    offset: usize,
    frames: usize,
    timestamp: f64,
}
#[derive(Clone)]
enum Control {
    Open,
    Finish(f64),
    Cancel,
}

#[derive(Clone, Debug)]
pub enum EncoderState {
    Preparing,
    Ready,
    Finalizing,
    Saved(PathBuf),
    Failed { error: String, incomplete: PathBuf },
    Cancelled,
}

pub struct Encoder {
    queue: Arc<MediaQueue<Video, Audio>>,
    control: Arc<ArcSwap<Control>>,
    state: Arc<ArcSwap<EncoderState>>,
    closed: bool,
}

impl Encoder {
    pub fn prepare(output: OutputSettings, reservation: OutputReservation) -> Self {
        let queue = Arc::new(MediaQueue::new(2, 128));
        let control = Arc::new(ArcSwap::from_pointee(Control::Open));
        let state = Arc::new(ArcSwap::from_pointee(EncoderState::Preparing));
        let (q, c, s) = (queue.clone(), control.clone(), state.clone());
        std::thread::spawn(move || {
            let incomplete = reservation.staging_path().to_owned();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(output, reservation, &q, &c, &s)
            }));
            if result.is_err() {
                q.fail("Media writer worker panicked".into());
                s.store(Arc::new(EncoderState::Failed {
                    error: "Media writer worker panicked".into(),
                    incomplete,
                }));
            }
        });
        Self {
            queue,
            control,
            state,
            closed: false,
        }
    }

    pub fn state(&self) -> EncoderState {
        (**self.state.load()).clone()
    }
    pub fn error(&self) -> Option<String> {
        self.queue.error().map(str::to_owned)
    }
    pub fn stats(&self) -> QueueStats {
        self.queue.stats()
    }
    pub fn video(&mut self, frame: RenderedFrame, timestamp: f64) -> bool {
        !self.closed && self.queue.video(Video { frame, timestamp })
    }
    pub fn audio(
        &mut self,
        sample: NativeSample,
        offset: usize,
        frames: usize,
        timestamp: f64,
    ) -> Result<(), String> {
        if self.closed {
            return Err("Writer input is closed".into());
        }
        self.queue.audio(Audio {
            sample,
            offset,
            frames,
            timestamp,
        })
    }
    pub fn finish(&mut self, end: f64) {
        if !self.closed {
            self.closed = true;
            self.control.store(Arc::new(Control::Finish(end)));
        }
    }
    pub fn cancel(&mut self) {
        if !self.closed {
            self.closed = true;
            self.control.store(Arc::new(Control::Cancel));
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn run(
    output: OutputSettings,
    reservation: OutputReservation,
    queue: &MediaQueue<Video, Audio>,
    control: &ArcSwap<Control>,
    state: &ArcSwap<EncoderState>,
) {
    let incomplete = reservation.staging_path().to_owned();
    let mut writer = match Writer::prepare(&incomplete, &output) {
        Ok(writer) => writer,
        Err(error) => {
            let error = match reservation.abort() {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; rollback: {cleanup}"),
            };
            queue.fail(error.clone());
            state.store(Arc::new(EncoderState::Failed { error, incomplete }));
            return;
        }
    };
    state.store(Arc::new(EncoderState::Ready));
    let mut pending_audio: Option<(Audio, Instant)> = None;
    let mut native_failure = false;
    loop {
        let command = control.load_full();
        if matches!(*command, Control::Cancel) {
            writer.cancel();
            match reservation.abort() {
                Ok(()) => state.store(Arc::new(EncoderState::Cancelled)),
                Err(error) => state.store(Arc::new(EncoderState::Failed { error, incomplete })),
            }
            return;
        }
        if pending_audio.is_none() {
            pending_audio = queue.pop_audio().map(|audio| (audio, Instant::now()));
        }
        if let Some((audio, since)) = pending_audio.as_ref() {
            match writer.append_audio(&audio.sample, audio.offset, audio.frames, audio.timestamp) {
                Ok(true) => pending_audio = None,
                Ok(false) if since.elapsed() < Duration::from_secs(2) => {}
                Ok(false) => {
                    queue
                        .fail("Audio encoder stalled for two seconds; recording incomplete".into());
                    native_failure = true;
                }
                Err(error) => {
                    queue.fail(error);
                    native_failure = true;
                }
            }
        }
        if let Some(video) = queue.pop_video() {
            match writer.append_video(&video.frame, video.timestamp) {
                Ok(true) => {}
                Ok(false) => queue.note_video_drop(),
                Err(error) => {
                    queue.fail(error);
                    native_failure = true;
                }
            }
        }
        let stats = queue.stats();
        if let Control::Finish(end) = *command
            && (native_failure
                || (pending_audio.is_none() && stats.video_depth == 0 && stats.audio_depth == 0))
        {
            state.store(Arc::new(EncoderState::Finalizing));
            let result = finish(&mut writer, end);
            let prior_error = queue.error().map(str::to_owned);
            match result {
                Ok(()) if prior_error.is_none() => match reservation.promote() {
                    Ok(path) => state.store(Arc::new(EncoderState::Saved(path))),
                    Err(error) => {
                        queue.fail(error.clone());
                        state.store(Arc::new(EncoderState::Failed { error, incomplete }));
                    }
                },
                result => {
                    let error = match (prior_error, result.err()) {
                        (Some(a), Some(b)) => format!("{a}; finalization: {b}"),
                        (Some(a), None) | (None, Some(a)) => a,
                        _ => unreachable!(),
                    };
                    queue.fail(error.clone());
                    state.store(Arc::new(EncoderState::Failed { error, incomplete }));
                }
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn finish(writer: &mut Writer, end: f64) -> Result<(), String> {
    let handle = writer.finish(end)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(result) = handle.poll() {
            return result;
        }
        if Instant::now() >= deadline {
            return Err("Writer finalization timed out; incomplete artifact retained".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::naming::TemplateContext;

    fn reservation(directory: &std::path::Path) -> OutputReservation {
        OutputReservation::reserve(
            directory,
            "{counter}.mp4",
            &TemplateContext {
                name: "encoder test".into(),
                timestamp: time::OffsetDateTime::now_utc(),
                counter: 1,
            },
        )
        .unwrap()
    }

    fn wait(encoder: &Encoder, done: impl Fn(&EncoderState) -> bool) -> EncoderState {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let state = encoder.state();
            if done(&state) {
                return state;
            }
            assert!(Instant::now() < deadline, "Encoder timeout: {state:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn invalid_format_is_reported_asynchronously_without_publishing_output() {
        let directory = tempfile::tempdir().unwrap();
        let encoder = Encoder::prepare(
            OutputSettings {
                fps: 0,
                ..Default::default()
            },
            reservation(directory.path()),
        );
        assert!(matches!(
            wait(&encoder, |s| matches!(s, EncoderState::Failed { .. })),
            EncoderState::Failed { .. }
        ));
        assert!(encoder.error().is_some());
        assert!(!directory.path().join("1.mp4").exists());
    }

    #[test]
    fn preparation_cancellation_is_asynchronous_and_removes_staging() {
        let directory = tempfile::tempdir().unwrap();
        let mut encoder =
            Encoder::prepare(OutputSettings::default(), reservation(directory.path()));
        let state = wait(&encoder, |s| !matches!(s, EncoderState::Preparing));
        assert!(matches!(state, EncoderState::Ready), "{state:?}");
        encoder.cancel();
        encoder.cancel();
        assert!(matches!(
            wait(&encoder, |s| matches!(
                s,
                EncoderState::Cancelled | EncoderState::Failed { .. }
            )),
            EncoderState::Cancelled
        ));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn empty_recording_cannot_report_a_successful_save() {
        let directory = tempfile::tempdir().unwrap();
        let mut encoder =
            Encoder::prepare(OutputSettings::default(), reservation(directory.path()));
        assert!(matches!(
            wait(&encoder, |s| !matches!(s, EncoderState::Preparing)),
            EncoderState::Ready
        ));
        encoder.finish(1.0);
        encoder.finish(2.0);
        let state = wait(&encoder, |s| {
            matches!(s, EncoderState::Saved(_) | EncoderState::Failed { .. })
        });
        assert!(matches!(state, EncoderState::Failed { .. }), "{state:?}");
        assert!(!directory.path().join("1.mp4").exists());
    }
}
