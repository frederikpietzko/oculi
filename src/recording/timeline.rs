//! Shared host-clock timing in seconds. Active ranges are half-open; PCM frames
//! are intervals and are retained only when the entire interval is active.

#[derive(Clone, Debug)]
struct ActiveSpan {
    start: f64,
    end: Option<f64>,
    timestamp: f64,
}

#[derive(Clone, Debug)]
pub struct Timeline {
    spans: Vec<ActiveSpan>,
    last_control: f64,
    stopped: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioSpan {
    /// Offset and length in PCM frames, not interleaved channel values.
    pub offset: usize,
    pub frames: usize,
    pub timestamp: f64,
}

impl Timeline {
    pub fn new(start: f64) -> Result<Self, String> {
        finite(start, "start")?;
        Ok(Self {
            spans: vec![ActiveSpan {
                start,
                end: None,
                timestamp: 0.0,
            }],
            last_control: start,
            stopped: false,
        })
    }

    /// Repeated controls are no-ops, but must still have ordered timestamps.
    pub fn pause(&mut self, host: f64) -> Result<(), String> {
        self.validate_control(host)?;
        if self.stopped {
            return Err("cannot pause a stopped timeline".into());
        }
        let span = self.spans.last_mut().unwrap();
        if span.end.is_none() {
            span.end = Some(host);
        }
        self.last_control = host;
        Ok(())
    }

    pub fn resume(&mut self, host: f64) -> Result<(), String> {
        self.validate_control(host)?;
        if self.stopped {
            return Err("cannot resume a stopped timeline".into());
        }
        let span = self.spans.last_mut().unwrap();
        if let Some(end) = span.end {
            if end == host {
                // A zero-length pause must not split a complete PCM sample.
                span.end = None;
            } else {
                let timestamp = span.timestamp + (end - span.start);
                self.spans.push(ActiveSpan {
                    start: host,
                    end: None,
                    timestamp,
                });
            }
        }
        self.last_control = host;
        Ok(())
    }

    pub fn stop(&mut self, host: f64) -> Result<(), String> {
        self.validate_control(host)?;
        if !self.stopped {
            let span = self.spans.last_mut().unwrap();
            if span.end.is_none() {
                span.end = Some(host);
            }
            self.stopped = true;
        }
        self.last_control = host;
        Ok(())
    }

    /// Maps capture time, not callback arrival time. Historical samples remain
    /// valid after subsequent controls; start/resume are included, pause/stop not.
    pub fn map(&self, host: f64) -> Option<f64> {
        if !host.is_finite() {
            return None;
        }
        let span = self.span_at(host)?;
        if span.end.is_some_and(|end| host >= end) {
            return None;
        }
        let timestamp = span.timestamp + (host - span.start);
        timestamp.is_finite().then_some(timestamp)
    }

    /// Active duration at `host`, including history and frozen during pauses or
    /// after stop. Nonfinite queries return zero; unrepresentable duration saturates.
    pub fn elapsed(&self, host: f64) -> f64 {
        if !host.is_finite() {
            return 0.0;
        }
        self.span_at(host).map_or(0.0, |span| {
            let end = span.end.map_or(host, |end| host.min(end));
            (span.timestamp + (end - span.start)).min(f64::MAX)
        })
    }

    /// Returns disjoint slices of the original PCM buffer without inventing a
    /// new track origin. Each frame covers `[start + i/rate, start + (i+1)/rate)`.
    /// Partial frames at either boundary are discarded, not rounded into a pause.
    pub fn audio_spans(
        &self,
        start: f64,
        frames: usize,
        rate: u32,
    ) -> Result<Vec<AudioSpan>, String> {
        finite(start, "audio start")?;
        if rate == 0 {
            return Err("audio sample rate must be positive".into());
        }
        if frames as u128 > (1_u128 << 53) {
            return Err("audio frame count exceeds clock precision".into());
        }
        if frames == 0 {
            return Ok(Vec::new());
        }
        let rate = f64::from(rate);
        let edge = |index: usize| start + index as f64 / rate;
        let end = edge(frames);
        finite(end, "audio end")?;
        if edge(1) <= start || edge(frames - 1) >= end {
            return Err("host clock cannot resolve audio sample intervals".into());
        }
        let mut result = Vec::new();
        for span in &self.spans {
            if span.start >= end {
                break;
            }
            if span.end.is_some_and(|end| end <= start) {
                continue;
            }
            // Search reconstructed sample edges instead of ceil((boundary-start)
            // * rate): subtraction can turn an exact fractional edge into 3.000…1.
            let offset = first_index(frames, |index| edge(index) >= span.start);
            let limit = span.end.map_or(frames, |end| {
                first_index(frames, |index| edge(index + 1) > end)
            });
            if limit > offset {
                let timestamp = span.timestamp + (edge(offset) - span.start);
                finite(timestamp, "audio timestamp")?;
                result.push(AudioSpan {
                    offset,
                    frames: limit - offset,
                    timestamp,
                });
            }
        }
        Ok(result)
    }

    fn span_at(&self, host: f64) -> Option<&ActiveSpan> {
        let index = self.spans.partition_point(|span| span.start <= host);
        index.checked_sub(1).map(|index| &self.spans[index])
    }

    fn validate_control(&self, host: f64) -> Result<(), String> {
        finite(host, "control time")?;
        if host < self.last_control {
            return Err("timeline control time moved backwards".into());
        }
        finite(host - self.spans[0].start, "timeline duration")
    }
}

fn first_index(limit: usize, predicate: impl Fn(usize) -> bool) -> usize {
    let (mut low, mut high) = (0, limit);
    while low < high {
        let middle = low + (high - low) / 2;
        if predicate(middle) {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    low
}

/// Call with the shared timeline's elapsed seconds, independently of capture
/// callbacks, to sample the last valid video frame even when the screen is static.
#[derive(Clone, Debug)]
pub struct FrameCadence {
    fps: u32,
    last_slot: Option<u64>,
    last_elapsed: f64,
    dropped: u64,
}

impl FrameCadence {
    pub fn new(fps: u32) -> Result<Self, String> {
        if fps == 0 {
            return Err("frame rate must be positive".into());
        }
        Ok(Self {
            fps,
            last_slot: None,
            last_elapsed: 0.0,
            dropped: 0,
        })
    }

    /// Emits only the current slot. Invalid or backwards times do not mutate
    /// cadence; missed slots are counted rather than returned as a backlog.
    pub fn due(&mut self, elapsed: f64) -> Option<f64> {
        if !elapsed.is_finite() || elapsed < self.last_elapsed {
            return None;
        }
        let fps = f64::from(self.fps);
        let scaled = elapsed * fps;
        // Beyond this range f64 cannot distinguish adjacent integral slots.
        if scaled >= (1_u64 << 53) as f64 {
            return None;
        }
        let mut slot = scaled.floor() as u64;
        if (slot + 1) as f64 / fps <= elapsed {
            slot += 1;
        } else if slot as f64 / fps > elapsed {
            slot = slot.saturating_sub(1);
        }
        self.last_elapsed = elapsed;
        if self.last_slot.is_some_and(|last| slot <= last) {
            return None;
        }
        let next = self.last_slot.map_or(0, |last| last + 1);
        self.dropped = self.dropped.saturating_add(slot - next);
        self.last_slot = Some(slot);
        Some(slot as f64 / fps)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// Optional affine mapping for a clock not already expressed in host seconds.
/// Origins must be a correlated clock pair supplied by the platform, never the
/// first sample or its queue arrival. Host-clock capture needs no mapper.
/// Calibration is fixed: excessive relative progression drift is an error, not
/// a timestamp jump or an independently reset audio/video origin.
#[derive(Clone, Debug)]
pub struct ClockMapper {
    device_origin: f64,
    host_origin: f64,
    rate_ratio: f64,
    max_drift: f64,
    last_device: f64,
    last_host: f64,
}

impl ClockMapper {
    /// `rate_ratio` is host seconds per device second; `max_drift` is seconds.
    pub fn new(
        device_origin: f64,
        host_origin: f64,
        rate_ratio: f64,
        max_drift: f64,
    ) -> Result<Self, String> {
        finite(device_origin, "device origin")?;
        finite(host_origin, "host origin")?;
        finite(rate_ratio, "clock rate ratio")?;
        finite(max_drift, "maximum clock drift")?;
        if rate_ratio <= 0.0 || max_drift < 0.0 {
            return Err("clock rate must be positive and drift bound nonnegative".into());
        }
        Ok(Self {
            device_origin,
            host_origin,
            rate_ratio,
            max_drift,
            last_device: device_origin,
            last_host: host_origin,
        })
    }

    pub fn map(&self, device: f64) -> Result<f64, String> {
        finite(device, "device time")?;
        let host = self.host_origin + (device - self.device_origin) * self.rate_ratio;
        finite(host, "mapped host time")?;
        Ok(host)
    }

    /// Observes another correlated clock pair, not a callback arrival time.
    /// Returns signed device-minus-host progression drift in seconds. Compare
    /// against the original calibration so gradual drift cannot go unnoticed.
    pub fn observe(&mut self, device: f64, host: f64) -> Result<f64, String> {
        finite(device, "device observation")?;
        finite(host, "host observation")?;
        if device < self.last_device || host < self.last_host {
            return Err("clock observation moved backwards".into());
        }
        let drift = (device - self.device_origin) * self.rate_ratio - (host - self.host_origin);
        finite(drift, "clock drift")?;
        if drift.abs() > self.max_drift {
            return Err(format!(
                "excessive clock drift: {drift:.9}s exceeds {:.9}s",
                self.max_drift
            ));
        }
        self.last_device = device;
        self.last_host = host;
        Ok(drift)
    }
}

fn finite(value: f64, name: &str) -> Result<(), String> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(format!("{name} must be finite"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
    }

    #[test]
    fn common_start_preserves_track_offsets_and_recording_independence() {
        let mut first = Timeline::new(100.0).unwrap();
        let second = first.clone();
        assert_eq!(first.map(99.0), None);
        assert_eq!(first.map(100.0), Some(0.0));
        assert_eq!(first.map(101.0), Some(1.0));
        assert_eq!(second.map(103.0), Some(3.0));
        assert_eq!(second.audio_spans(102.0, 10, 10).unwrap()[0].timestamp, 2.0);
        first.pause(104.0).unwrap();
        assert_eq!(first.map(105.0), None);
        assert_eq!(second.map(105.0), Some(5.0));
    }

    #[test]
    fn historical_samples_survive_multiple_pauses_and_stop() {
        let mut timeline = Timeline::new(10.0).unwrap();
        timeline.resume(10.0).unwrap();
        timeline.pause(12.0).unwrap();
        timeline.pause(13.0).unwrap();
        timeline.resume(14.0).unwrap();
        timeline.resume(15.0).unwrap();
        timeline.pause(16.0).unwrap();
        timeline.resume(18.0).unwrap();
        timeline.stop(20.0).unwrap();
        timeline.stop(21.0).unwrap();
        for (host, mapped, elapsed) in [
            (9.0, None, 0.0),
            (10.0, Some(0.0), 0.0),
            (11.0, Some(1.0), 1.0),
            (12.0, None, 2.0),
            (13.0, None, 2.0),
            (14.0, Some(2.0), 2.0),
            (15.0, Some(3.0), 3.0),
            (16.0, None, 4.0),
            (18.0, Some(4.0), 4.0),
            (19.0, Some(5.0), 5.0),
            (20.0, None, 6.0),
            (25.0, None, 6.0),
        ] {
            assert_eq!(timeline.map(host), mapped, "host {host}");
            assert_eq!(timeline.elapsed(host), elapsed, "host {host}");
        }
        assert!(timeline.pause(22.0).is_err());
        assert!(timeline.resume(22.0).is_err());
    }

    #[test]
    fn stop_while_paused_does_not_count_pause_time() {
        let mut timeline = Timeline::new(10.0).unwrap();
        timeline.pause(12.0).unwrap();
        timeline.stop(18.0).unwrap();
        assert_eq!(timeline.elapsed(100.0), 2.0);
        assert_eq!(timeline.map(11.0), Some(1.0));
        assert_eq!(timeline.map(12.0), None);
        assert_eq!(timeline.map(19.0), None);
        assert_eq!(timeline.audio_spans(10.0, 200, 10).unwrap()[0].frames, 20);
    }

    #[test]
    fn zero_duration_pause_and_immediate_stop() {
        let mut timeline = Timeline::new(0.0).unwrap();
        timeline.pause(0.15).unwrap();
        timeline.resume(0.15).unwrap();
        assert_eq!(timeline.audio_spans(0.0, 3, 10).unwrap()[0].frames, 3);
        let mut empty = Timeline::new(2.0).unwrap();
        empty.pause(2.0).unwrap();
        empty.stop(2.0).unwrap();
        assert_eq!(empty.map(2.0), None);
        assert!(empty.audio_spans(1.0, 30, 10).unwrap().is_empty());
        assert_eq!(empty.elapsed(3.0), 0.0);
    }

    #[test]
    fn controls_reject_backwards_nonfinite_and_overflow_without_mutating() {
        let mut timeline = Timeline::new(10.0).unwrap();
        timeline.pause(12.0).unwrap();
        timeline.pause(14.0).unwrap();
        for invalid in [9.0, 13.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(timeline.pause(invalid).is_err());
            assert!(timeline.resume(invalid).is_err());
            assert!(timeline.stop(invalid).is_err());
        }
        timeline.resume(14.0).unwrap();
        assert_eq!(timeline.map(15.0), Some(3.0));
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(Timeline::new(invalid).is_err());
            assert_eq!(timeline.map(invalid), None);
            assert_eq!(timeline.elapsed(invalid), 0.0);
        }
        let mut extreme = Timeline::new(-f64::MAX).unwrap();
        assert!(extreme.stop(f64::MAX).is_err());
        assert_eq!(extreme.map(f64::MAX), None);
        assert_eq!(extreme.elapsed(f64::MAX), f64::MAX);
    }

    #[test]
    fn audio_crosses_common_start_pause_resume_and_stop() {
        let mut timeline = Timeline::new(10.25).unwrap();
        timeline.pause(10.75).unwrap();
        timeline.resume(11.25).unwrap();
        timeline.stop(11.75).unwrap();
        assert_eq!(
            timeline.audio_spans(10.0, 20, 8).unwrap(),
            vec![
                AudioSpan {
                    offset: 2,
                    frames: 4,
                    timestamp: 0.0
                },
                AudioSpan {
                    offset: 10,
                    frames: 4,
                    timestamp: 0.5
                },
            ]
        );
    }

    #[test]
    fn fractional_edges_do_not_lose_exact_samples() {
        for origin in [0.0, 10.0, 123456.0] {
            let mut timeline = Timeline::new(origin + 0.3).unwrap();
            timeline.pause(origin + 0.6).unwrap();
            timeline.resume(origin + 0.7).unwrap();
            timeline.stop(origin + 0.9).unwrap();
            let spans = timeline.audio_spans(origin, 10, 10).unwrap();
            assert_eq!(spans.len(), 2);
            assert_eq!((spans[0].offset, spans[0].frames), (3, 3));
            assert_eq!((spans[1].offset, spans[1].frames), (7, 2));
            close(spans[0].timestamp, 0.0);
            close(spans[1].timestamp, 0.3);
        }
    }

    #[test]
    fn fractional_sample_intervals_are_not_rounded_into_active_ranges() {
        let mut timeline = Timeline::new(0.125).unwrap();
        timeline.pause(0.375).unwrap();
        timeline.resume(0.625).unwrap();
        timeline.stop(0.875).unwrap();
        assert!(timeline.audio_spans(0.0, 4, 4).unwrap().is_empty());
        let mut timeline = Timeline::new(0.125).unwrap();
        timeline.pause(0.625).unwrap();
        timeline.resume(0.75).unwrap();
        timeline.stop(1.125).unwrap();
        assert_eq!(
            timeline.audio_spans(0.0, 8, 4).unwrap(),
            vec![
                AudioSpan {
                    offset: 1,
                    frames: 1,
                    timestamp: 0.125
                },
                AudioSpan {
                    offset: 3,
                    frames: 1,
                    timestamp: 0.5
                },
            ]
        );
    }

    #[test]
    fn adjacent_buffers_and_ranges_never_duplicate_samples() {
        let mut timeline = Timeline::new(0.0).unwrap();
        timeline.pause(0.5).unwrap();
        timeline.resume(0.625).unwrap();
        timeline.stop(1.0).unwrap();
        let whole = timeline.audio_spans(0.0, 8, 8).unwrap();
        let left = timeline.audio_spans(0.0, 4, 8).unwrap();
        let right = timeline.audio_spans(0.5, 4, 8).unwrap();
        assert_eq!(whole[0], left[0]);
        assert_eq!(whole[1].offset, right[0].offset + 4);
        assert_eq!(whole[1].frames, right[0].frames);
        assert_eq!(whole[1].timestamp, right[0].timestamp);
        assert!(whole[0].offset + whole[0].frames <= whole[1].offset);
        assert_eq!(whole.iter().map(|span| span.frames).sum::<usize>(), 7);
    }

    #[test]
    fn audio_at_realistic_host_origin_and_sample_rate() {
        let origin = 123456.75;
        for rate in [44_100, 48_000, 96_000] {
            let edge = |i| origin + f64::from(i) / f64::from(rate);
            let mut timeline = Timeline::new(edge(7)).unwrap();
            timeline.pause(edge(101)).unwrap();
            timeline.resume(edge(202)).unwrap();
            timeline.stop(edge(409)).unwrap();
            let spans = timeline.audio_spans(origin, 512, rate).unwrap();
            assert_eq!((spans[0].offset, spans[0].frames), (7, 94));
            assert_eq!((spans[1].offset, spans[1].frames), (202, 207));
        }
    }

    #[test]
    fn audio_invalids_empty_buffers_and_outside_timeline() {
        let mut timeline = Timeline::new(10.0).unwrap();
        timeline.pause(12.0).unwrap();
        timeline.resume(14.0).unwrap();
        timeline.stop(16.0).unwrap();
        for start in [0.0, 12.0, 16.0] {
            assert!(timeline.audio_spans(start, 10, 10).unwrap().is_empty());
        }
        assert!(timeline.audio_spans(11.0, 0, 10).unwrap().is_empty());
        assert!(timeline.audio_spans(11.0, 10, 0).is_err());
        assert!(timeline.audio_spans(f64::NAN, 10, 10).is_err());
        assert!(timeline.audio_spans(f64::INFINITY, 0, 10).is_err());
        assert!(timeline.audio_spans(f64::MAX, 10, 48_000).is_err());
        assert!(timeline.audio_spans(10.0, usize::MAX, 48_000).is_err());
    }

    #[test]
    fn mapped_samples_and_elapsed_are_monotonic_across_history() {
        let mut timeline = Timeline::new(0.25).unwrap();
        for (pause, resume) in [(1.0, 2.0), (3.0, 4.0), (5.0, 7.0)] {
            timeline.pause(pause).unwrap();
            timeline.resume(resume).unwrap();
        }
        timeline.stop(8.0).unwrap();
        let mut last_timestamp = -1.0;
        let mut last_elapsed = 0.0;
        for tick in 0..1000 {
            let host = f64::from(tick) / 100.0;
            let elapsed = timeline.elapsed(host);
            assert!(elapsed >= last_elapsed);
            last_elapsed = elapsed;
            if let Some(timestamp) = timeline.map(host) {
                assert!(timestamp > last_timestamp);
                last_timestamp = timestamp;
                close(timestamp, elapsed);
            }
        }
    }

    #[test]
    fn complete_audio_intervals_match_exhaustive_boundary_oracle() {
        for start in 0..8 {
            for pause in start..12 {
                for resume in pause + 1..16 {
                    let mut timeline = Timeline::new(f64::from(start) / 8.0).unwrap();
                    timeline.pause(f64::from(pause) / 8.0).unwrap();
                    timeline.resume(f64::from(resume) / 8.0).unwrap();
                    timeline.stop(2.0).unwrap();
                    let spans = timeline.audio_spans(0.0, 10, 4).unwrap();
                    let actual: Vec<_> = spans
                        .iter()
                        .flat_map(|span| span.offset..span.offset + span.frames)
                        .collect();
                    let expected: Vec<_> = (0..10)
                        .filter(|&i| {
                            let low = i as f64 / 4.0;
                            let high = (i + 1) as f64 / 4.0;
                            (low >= f64::from(start) / 8.0 && high <= f64::from(pause) / 8.0)
                                || (low >= f64::from(resume) / 8.0 && high <= 2.0)
                        })
                        .collect();
                    assert_eq!(actual, expected);
                    for span in spans {
                        close(
                            span.timestamp,
                            timeline.map(span.offset as f64 / 4.0).unwrap(),
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn cadence_samples_static_frame_without_backlog() {
        let mut cadence = FrameCadence::new(10).unwrap();
        assert_eq!(cadence.due(0.0), Some(0.0));
        assert_eq!(cadence.due(0.05), None);
        assert_eq!(cadence.due(0.1), Some(0.1));
        assert_eq!(cadence.due(0.59), Some(0.5));
        assert_eq!(cadence.dropped(), 3);
        assert_eq!(cadence.due(0.59), None);
        assert_eq!(cadence.due(0.6), Some(0.6));
        assert_eq!(cadence.dropped(), 3);
        let mut late = FrameCadence::new(10).unwrap();
        assert_eq!(late.due(0.5), Some(0.5));
        assert_eq!(late.dropped(), 5);
    }

    #[test]
    fn cadence_fractional_boundaries_and_invalids() {
        assert!(FrameCadence::new(0).is_err());
        for fps in [24, 30, 60, 120] {
            let mut cadence = FrameCadence::new(fps).unwrap();
            for slot in 0..1000 {
                let elapsed = f64::from(slot) / f64::from(fps);
                assert_eq!(cadence.due(elapsed), Some(elapsed));
                assert_eq!(cadence.due(elapsed), None);
            }
            assert_eq!(cadence.dropped(), 0);
        }
        let mut cadence = FrameCadence::new(10).unwrap();
        for invalid in [-1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::MAX] {
            assert_eq!(cadence.due(invalid), None);
        }
        assert_eq!(cadence.due(0.0), Some(0.0));
        assert_eq!(cadence.due(0.39), Some(0.3));
        assert_eq!(cadence.due(0.35), None);
        assert_eq!(cadence.due(0.4), Some(0.4));
        assert_eq!(cadence.dropped(), 2);
    }

    #[test]
    fn pause_elapsed_does_not_create_cadence_backlog() {
        let mut timeline = Timeline::new(10.0).unwrap();
        let mut cadence = FrameCadence::new(4).unwrap();
        assert_eq!(cadence.due(timeline.elapsed(10.0)), Some(0.0));
        timeline.pause(10.125).unwrap();
        assert_eq!(cadence.due(timeline.elapsed(20.0)), None);
        timeline.resume(20.0).unwrap();
        assert_eq!(cadence.due(timeline.elapsed(20.125)), Some(0.25));
        assert_eq!(cadence.dropped(), 0);
    }

    #[test]
    fn calibrated_clocks_preserve_common_origin_and_rate() {
        let timeline = Timeline::new(1000.0).unwrap();
        let mut audio = ClockMapper::new(500.0, 1000.0, 0.5, 0.001).unwrap();
        let mut video = ClockMapper::new(20.0, 1000.0, 1.0, 0.001).unwrap();
        assert_eq!(timeline.map(audio.map(502.0).unwrap()), Some(1.0));
        assert_eq!(timeline.map(video.map(23.0).unwrap()), Some(3.0));
        assert_eq!(audio.observe(502.0, 1001.0).unwrap(), 0.0);
        assert_eq!(video.observe(23.0, 1003.0).unwrap(), 0.0);
        // Irregular observation spacing is not a clock rate change.
        assert_eq!(audio.observe(540.0, 1020.0).unwrap(), 0.0);
        assert_eq!(audio.observe(540.0, 1020.0).unwrap(), 0.0);
        assert_eq!(audio.map(502.0).unwrap(), 1001.0);
    }

    #[test]
    fn drift_is_cumulative_bounded_and_never_rezeros_tracks() {
        let mut clock = ClockMapper::new(0.0, 100.0, 1.0, 0.01).unwrap();
        close(clock.observe(10.005, 110.0).unwrap(), 0.005);
        assert!(
            clock
                .observe(20.02, 120.0)
                .unwrap_err()
                .contains("excessive clock drift")
        );
        close(clock.map(20.02).unwrap(), 120.02);
        close(clock.observe(15.007, 115.0).unwrap(), 0.007);
        assert!(clock.observe(14.0, 116.0).is_err());
        assert!(clock.observe(16.0, 114.0).is_err());
        let mut exact = ClockMapper::new(0.0, 0.0, 1.0, 0.125).unwrap();
        assert_eq!(exact.observe(1.125, 1.0).unwrap(), 0.125);
        assert_eq!(exact.observe(2.0, 2.125).unwrap(), -0.125);
        assert!(exact.observe(3.0, 3.25).is_err());
    }

    #[test]
    fn clock_invalid_calibration_observations_and_overflow() {
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(ClockMapper::new(invalid, 0.0, 1.0, 0.1).is_err());
            assert!(ClockMapper::new(0.0, invalid, 1.0, 0.1).is_err());
            assert!(ClockMapper::new(0.0, 0.0, invalid, 0.1).is_err());
            assert!(ClockMapper::new(0.0, 0.0, 1.0, invalid).is_err());
            let mut clock = ClockMapper::new(0.0, 0.0, 1.0, 0.1).unwrap();
            assert!(clock.map(invalid).is_err());
            assert!(clock.observe(invalid, 0.0).is_err());
            assert!(clock.observe(0.0, invalid).is_err());
        }
        assert!(ClockMapper::new(0.0, 0.0, 0.0, 0.1).is_err());
        assert!(ClockMapper::new(0.0, 0.0, -1.0, 0.1).is_err());
        assert!(ClockMapper::new(0.0, 0.0, 1.0, -0.1).is_err());
        let mut clock = ClockMapper::new(0.0, 0.0, 2.0, 0.1).unwrap();
        assert!(clock.map(f64::MAX).is_err());
        assert!(clock.observe(f64::MAX, f64::MAX).is_err());
    }
}
