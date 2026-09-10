//! Serialized group lifecycle. Native preparation and finalization are polled,
//! never awaited on the UI or capture thread.

use super::timeline::Timeline;
use crate::model::{GroupCommand, GroupState, RecordingConfig, RecordingId};
use std::{collections::HashSet, path::PathBuf};

#[derive(Clone, Debug)]
pub struct RecordingReport {
    pub id: RecordingId,
    pub name: String,
    pub status: String,
    pub saved_path: Option<PathBuf>,
    pub error: Option<String>,
    pub audio_level: f32,
    pub dropped_video: u64,
    pub video_queue: usize,
    pub audio_queue: usize,
}

#[derive(Clone, Debug, Default)]
pub struct GroupSnapshot {
    /// Assigned by the serialized runtime after fully processing a command.
    pub command_sequence: u64,
    pub state: GroupState,
    pub elapsed: f64,
    pub recordings: Vec<RecordingReport>,
    pub error: Option<String>,
}

impl GroupSnapshot {
    pub fn active(&self) -> bool {
        active(self.state)
    }
}

pub trait GroupBackend {
    /// May partially prepare before returning Err; rollback is always requested.
    fn prepare(&mut self, configs: &[RecordingConfig]) -> Result<(), String>;
    fn prepared(&mut self) -> Result<bool, String>;
    fn set_timeline(&mut self, timeline: Timeline);
    /// Drain previously accepted input, then asynchronously finish every writer.
    fn begin_finish(&mut self, end: f64);
    fn begin_rollback(&mut self);
    /// True only after all writers are terminal and owned input leases released.
    fn finalized(&mut self) -> bool;
    fn reports(&self) -> Vec<RecordingReport>;
    fn error(&self) -> Option<String>;
}

pub struct Coordinator<B> {
    backend: B,
    state: GroupState,
    timeline: Option<Timeline>,
    error: Option<String>,
    phase_started: f64,
    last_host: f64,
}

impl<B: GroupBackend> Coordinator<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            state: GroupState::Idle,
            timeline: None,
            error: None,
            phase_started: 0.0,
            last_host: 0.0,
        }
    }
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }
    pub fn is_active(&self) -> bool {
        active(self.state)
    }
    pub fn snapshot(&self, host: f64) -> GroupSnapshot {
        GroupSnapshot {
            command_sequence: 0,
            state: self.state,
            elapsed: self.timeline.as_ref().map_or(0.0, |t| t.elapsed(host)),
            recordings: self.backend.reports(),
            error: self.error.clone(),
        }
    }

    pub fn command(
        &mut self,
        command: GroupCommand,
        configs: Vec<RecordingConfig>,
        host: f64,
    ) -> Result<(), String> {
        self.check_clock(host)?;
        self.last_host = host;
        match command {
            GroupCommand::Start if !self.is_active() => {
                self.timeline = None;
                self.error = None;
                if let Err(error) = validate_group(&configs) {
                    self.state = GroupState::Failed;
                    self.error = Some(error.clone());
                    return Err(error);
                }
                self.state = GroupState::Preparing;
                self.phase_started = host;
                if let Err(error) = self.backend.prepare(&configs) {
                    self.error = Some(error.clone());
                    self.rollback(host);
                    return Err(error);
                }
            }
            GroupCommand::Start => {}
            GroupCommand::Pause if self.state == GroupState::Recording => {
                let timeline = self.timeline.as_mut().unwrap();
                timeline.pause(host)?;
                self.backend.set_timeline(timeline.clone());
                self.state = GroupState::Paused;
            }
            GroupCommand::Resume if self.state == GroupState::Paused => {
                let timeline = self.timeline.as_mut().unwrap();
                timeline.resume(host)?;
                self.backend.set_timeline(timeline.clone());
                self.state = GroupState::Recording;
            }
            GroupCommand::Stop => self.stop(host),
            GroupCommand::Pause | GroupCommand::Resume => {}
        }
        Ok(())
    }

    pub fn tick(&mut self, host: f64) {
        if let Err(error) = self.check_clock(host) {
            self.error.get_or_insert(error);
            self.stop(self.last_host);
            return;
        }
        self.last_host = host;
        match self.state {
            GroupState::Preparing => {
                let ready = self.backend.prepared();
                if let Some(error) = self
                    .backend
                    .error()
                    .or_else(|| ready.as_ref().err().cloned())
                {
                    self.error = Some(error);
                    self.rollback(host);
                } else if ready == Ok(true) {
                    match Timeline::new(host) {
                        Ok(timeline) => {
                            self.backend.set_timeline(timeline.clone());
                            self.timeline = Some(timeline);
                            self.state = GroupState::Recording;
                        }
                        Err(error) => {
                            self.error = Some(error);
                            self.rollback(host);
                        }
                    }
                } else if host - self.phase_started >= 15.0 {
                    self.error =
                        Some("Recording preparation timed out; rolling back all outputs".into());
                    self.rollback(host);
                }
            }
            GroupState::Recording | GroupState::Paused => {
                if let Some(error) = self.backend.error() {
                    self.error = Some(error);
                    self.stop(host);
                }
            }
            GroupState::Finalizing => {
                if let Some(error) = self.backend.error() {
                    self.error.get_or_insert(error);
                }
                if self.backend.finalized() {
                    self.state = if self.error.is_some()
                        || self.backend.reports().iter().any(|r| r.error.is_some())
                    {
                        GroupState::Failed
                    } else {
                        GroupState::Idle
                    };
                } else if host - self.phase_started >= 35.0 {
                    self.error.get_or_insert("Finalization is taking too long. Outputs remain locked until their workers finish; do not quit forcibly.".into());
                }
            }
            GroupState::Idle | GroupState::Failed => {}
        }
    }

    fn rollback(&mut self, host: f64) {
        self.backend.begin_rollback();
        self.state = GroupState::Finalizing;
        self.phase_started = host;
    }
    fn stop(&mut self, host: f64) {
        match self.state {
            GroupState::Preparing => self.rollback(host),
            GroupState::Recording | GroupState::Paused => {
                let timeline = self.timeline.as_mut().unwrap();
                if let Err(error) = timeline.stop(host) {
                    self.error.get_or_insert(error);
                }
                let end = timeline.elapsed(host);
                self.backend.set_timeline(timeline.clone());
                self.backend.begin_finish(end);
                self.state = GroupState::Finalizing;
                self.phase_started = host;
            }
            _ => {}
        }
    }
    fn check_clock(&self, host: f64) -> Result<(), String> {
        if !host.is_finite() || host < self.last_host {
            Err("Group clock is invalid or moved backwards".into())
        } else {
            Ok(())
        }
    }
}

fn active(state: GroupState) -> bool {
    matches!(
        state,
        GroupState::Preparing | GroupState::Recording | GroupState::Paused | GroupState::Finalizing
    )
}

fn validate_group(configs: &[RecordingConfig]) -> Result<(), String> {
    if configs.is_empty() {
        return Err("Configure at least one video/audio recording pair".into());
    }
    let mut ids = HashSet::new();
    for config in configs {
        if !ids.insert(config.id) {
            return Err("Recording IDs must be unique".into());
        }
        config
            .output
            .validate_format()
            .map_err(|e| format!("{}: {e}", config.name))?;
        crate::render::transform::validate(&config.viewport)
            .map_err(|e| format!("{}: {e}", config.name))?;
        if crate::capture::SourceKey::video(&config.video).is_none()
            || crate::capture::SourceKey::audio(&config.audio).is_none()
        {
            return Err(format!(
                "{}: select both a video source and an audio source",
                config.name
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AudioSourceSelector, VideoSourceSelector};

    #[derive(Default)]
    struct Fake {
        prepare_count: usize,
        ready: bool,
        prepare_error: Option<String>,
        error: Option<String>,
        rollback_count: usize,
        finish_ends: Vec<f64>,
        timelines: Vec<Timeline>,
        done: bool,
        reports: Vec<RecordingReport>,
    }
    impl GroupBackend for Fake {
        fn prepare(&mut self, configs: &[RecordingConfig]) -> Result<(), String> {
            self.prepare_count += 1;
            self.reports = configs
                .iter()
                .map(|c| RecordingReport {
                    id: c.id,
                    name: c.name.clone(),
                    status: "Preparing".into(),
                    saved_path: None,
                    error: None,
                    audio_level: 0.0,
                    dropped_video: 0,
                    video_queue: 0,
                    audio_queue: 0,
                })
                .collect();
            self.prepare_error.clone().map_or(Ok(()), Err)
        }
        fn prepared(&mut self) -> Result<bool, String> {
            Ok(self.ready)
        }
        fn set_timeline(&mut self, timeline: Timeline) {
            self.timelines.push(timeline);
        }
        fn begin_finish(&mut self, end: f64) {
            self.finish_ends.push(end);
        }
        fn begin_rollback(&mut self) {
            self.rollback_count += 1;
        }
        fn finalized(&mut self) -> bool {
            self.done
        }
        fn reports(&self) -> Vec<RecordingReport> {
            self.reports.clone()
        }
        fn error(&self) -> Option<String> {
            self.error.clone()
        }
    }
    fn configs() -> Vec<RecordingConfig> {
        let mut a = RecordingConfig::new("a");
        a.video = VideoSourceSelector::Display {
            display_id: crate::model::DisplayId(1),
            label: "display".into(),
        };
        a.audio = AudioSourceSelector::System;
        let mut b = a.duplicate();
        b.name = "b".into();
        vec![a, b]
    }
    fn running() -> Coordinator<Fake> {
        let mut c = Coordinator::new(Fake {
            ready: true,
            ..Default::default()
        });
        c.command(GroupCommand::Start, configs(), 100.0).unwrap();
        c.tick(101.0);
        assert_eq!(c.snapshot(101.0).state, GroupState::Recording);
        c
    }
    #[test]
    fn shared_commit_waits_for_all_writers_and_repeated_controls_are_idempotent() {
        let mut c = Coordinator::new(Fake::default());
        c.command(GroupCommand::Start, configs(), 100.0).unwrap();
        c.command(GroupCommand::Start, configs(), 100.1).unwrap();
        c.tick(101.0);
        assert_eq!(c.backend.prepare_count, 1);
        assert!(c.backend.timelines.is_empty());
        assert_eq!(c.snapshot(101.0).state, GroupState::Preparing);
        c.backend.ready = true;
        c.tick(102.0);
        assert_eq!(c.backend.timelines.len(), 1);
        assert_eq!(c.backend.timelines[0].map(102.5), Some(0.5));
        assert_eq!(c.snapshot(102.5).recordings.len(), 2);
        c.command(GroupCommand::Pause, vec![], 103.0).unwrap();
        c.command(GroupCommand::Pause, vec![], 104.0).unwrap();
        assert_eq!(c.snapshot(104.0).elapsed, 1.0);
        c.command(GroupCommand::Resume, vec![], 105.0).unwrap();
        c.command(GroupCommand::Resume, vec![], 105.5).unwrap();
        assert_eq!(c.backend.timelines.len(), 3);
        assert_eq!(c.backend.timelines.last().unwrap().map(105.5), Some(1.5));
        c.command(GroupCommand::Stop, vec![], 106.0).unwrap();
        c.command(GroupCommand::Stop, vec![], 107.0).unwrap();
        assert_eq!(c.backend.finish_ends, vec![2.0]);
        assert!(c.is_active());
        c.backend.done = true;
        c.tick(108.0);
        assert!(!c.is_active());
        assert_eq!(c.snapshot(110.0).elapsed, 2.0);
    }
    #[test]
    fn stopping_while_paused_omits_pause_and_preserves_historical_audio() {
        let mut c = running();
        c.command(GroupCommand::Pause, vec![], 103.0).unwrap();
        c.command(GroupCommand::Stop, vec![], 110.0).unwrap();
        assert_eq!(c.backend.finish_ends, vec![2.0]);
        let t = c.backend.timelines.last().unwrap();
        assert_eq!(t.map(102.0), Some(1.0));
        assert_eq!(t.map(104.0), None);
        let spans = t.audio_spans(102.5, 48000, 48000).unwrap();
        assert_eq!(spans[0].frames, 24000);
        assert_eq!(spans[0].timestamp, 1.5);
    }
    #[test]
    fn start_failure_rolls_back_every_prepared_output_before_unlocking() {
        let mut c = Coordinator::new(Fake {
            prepare_error: Some("second writer failed".into()),
            ..Default::default()
        });
        assert!(c.command(GroupCommand::Start, configs(), 100.0).is_err());
        assert_eq!(c.backend.rollback_count, 1);
        assert!(c.is_active());
        c.command(GroupCommand::Start, configs(), 101.0).unwrap();
        assert_eq!(c.backend.prepare_count, 1);
        c.backend.done = true;
        c.tick(102.0);
        assert_eq!(c.snapshot(102.0).state, GroupState::Failed);
        assert!(!c.is_active());
    }
    #[test]
    fn delayed_preparation_failure_and_timeout_never_commit() {
        for failure in [Some("source vanished".into()), None] {
            let mut c = Coordinator::new(Fake::default());
            c.command(GroupCommand::Start, configs(), 100.0).unwrap();
            c.backend.error = failure;
            c.tick(115.0);
            assert_eq!(c.backend.rollback_count, 1);
            assert!(c.backend.timelines.is_empty());
            assert!(c.is_active());
        }
    }
    #[test]
    fn stop_during_preparation_cancels_once_without_creating_a_timeline() {
        let mut c = Coordinator::new(Fake::default());
        c.command(GroupCommand::Start, configs(), 100.0).unwrap();
        c.command(GroupCommand::Stop, vec![], 101.0).unwrap();
        c.command(GroupCommand::Stop, vec![], 102.0).unwrap();
        assert_eq!(c.backend.rollback_count, 1);
        assert!(c.backend.finish_ends.is_empty());
        c.backend.done = true;
        c.tick(103.0);
        assert_eq!(c.snapshot(103.0).state, GroupState::Idle);
    }
    #[test]
    fn fatal_shared_source_or_writer_failure_finishes_all_partial_outputs() {
        for message in [
            "shared camera disconnected",
            "audio queue overflow",
            "disk write failed",
        ] {
            let mut c = running();
            c.backend.error = Some(message.into());
            c.tick(102.0);
            assert_eq!(c.backend.finish_ends, vec![1.0]);
            assert_eq!(c.snapshot(102.0).state, GroupState::Finalizing);
            c.backend.reports[0].saved_path = Some("a.mp4".into());
            c.backend.reports[1].error = Some(message.into());
            c.backend.done = true;
            c.tick(103.0);
            let snapshot = c.snapshot(103.0);
            assert_eq!(snapshot.state, GroupState::Failed);
            assert!(snapshot.recordings[0].saved_path.is_some());
            assert!(snapshot.recordings[1].error.is_some());
            assert_eq!(snapshot.error.as_deref(), Some(message));
        }
    }
    #[test]
    fn slow_finalization_stays_locked_instead_of_abandoning_writers() {
        let mut c = running();
        c.command(GroupCommand::Stop, vec![], 102.0).unwrap();
        c.tick(140.0);
        assert!(c.is_active());
        assert!(c.snapshot(140.0).error.unwrap().contains("too long"));
        c.command(GroupCommand::Start, configs(), 141.0).unwrap();
        assert_eq!(c.backend.prepare_count, 1);
        c.backend.done = true;
        c.tick(142.0);
        assert!(!c.is_active());
    }
    #[test]
    fn invalid_commands_configs_and_backwards_clocks_cannot_start_or_retime() {
        let mut c = Coordinator::new(Fake::default());
        assert!(c.command(GroupCommand::Start, vec![], 100.0).is_err());
        assert_eq!(c.backend.prepare_count, 0);
        let mut duplicate = configs();
        duplicate[1].id = duplicate[0].id;
        assert!(c.command(GroupCommand::Start, duplicate, 101.0).is_err());
        assert!(
            c.command(
                GroupCommand::Start,
                vec![RecordingConfig::new("empty")],
                102.0
            )
            .is_err()
        );
        assert_eq!(c.backend.prepare_count, 0);
        let mut c = running();
        assert!(c.command(GroupCommand::Pause, vec![], 100.0).is_err());
        assert_eq!(c.snapshot(101.0).state, GroupState::Recording);
        c.tick(f64::NAN);
        assert_eq!(c.backend.finish_ends, vec![0.0]);
        assert_eq!(c.snapshot(101.0).state, GroupState::Finalizing);
    }
}
