//! Bounded nonblocking handoffs. Video may drop; audio loss is always fatal.

use crossbeam_queue::ArrayQueue;
use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering},
};

pub struct MediaQueue<V, A> {
    video: ArrayQueue<V>,
    audio: ArrayQueue<A>,
    error: OnceLock<String>,
    dropped: AtomicU64,
    peak_video: AtomicU64,
    peak_audio: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct QueueStats {
    pub video_depth: usize,
    pub audio_depth: usize,
    pub peak_video: u64,
    pub peak_audio: u64,
    pub dropped_video: u64,
}

impl<V, A> MediaQueue<V, A> {
    pub fn new(video_capacity: usize, audio_capacity: usize) -> Self {
        Self {
            video: ArrayQueue::new(video_capacity),
            audio: ArrayQueue::new(audio_capacity),
            error: OnceLock::new(),
            dropped: AtomicU64::new(0),
            peak_video: AtomicU64::new(0),
            peak_audio: AtomicU64::new(0),
        }
    }
    pub fn video(&self, value: V) -> bool {
        if self.error.get().is_some() {
            return false;
        }
        if self.video.push(value).is_err() {
            self.note_video_drop();
            return false;
        }
        self.peak_video
            .fetch_max(self.video.len() as u64, Ordering::Relaxed);
        true
    }
    pub fn audio(&self, value: A) -> Result<(), String> {
        if let Some(error) = self.error.get() {
            return Err(error.clone());
        }
        if self.audio.push(value).is_err() {
            self.fail("Audio writer queue overflow; stop and finalize the group".into());
            return Err(self.error.get().unwrap().clone());
        }
        self.peak_audio
            .fetch_max(self.audio.len() as u64, Ordering::Relaxed);
        Ok(())
    }
    pub fn pop_video(&self) -> Option<V> {
        self.video.pop()
    }
    pub fn pop_audio(&self) -> Option<A> {
        self.audio.pop()
    }
    pub fn note_video_drop(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }
    pub fn fail(&self, error: String) {
        let _ = self.error.set(error);
    }
    pub fn error(&self) -> Option<&str> {
        self.error.get().map(String::as_str)
    }
    pub fn stats(&self) -> QueueStats {
        QueueStats {
            video_depth: self.video.len(),
            audio_depth: self.audio.len(),
            peak_video: self.peak_video.load(Ordering::Relaxed),
            peak_audio: self.peak_audio.load(Ordering::Relaxed),
            dropped_video: self.dropped.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn video_is_bounded_and_audio_overflow_stays_fatal_after_drain() {
        let q = MediaQueue::new(2, 2);
        assert!(q.video(1));
        assert!(q.video(2));
        assert!(!q.video(3));
        assert_eq!(q.stats().dropped_video, 1);
        assert_eq!(q.pop_video(), Some(1));
        assert_eq!(q.pop_video(), Some(2));
        q.audio(1).unwrap();
        q.audio(2).unwrap();
        assert!(q.audio(3).is_err());
        assert_eq!(q.pop_audio(), Some(1));
        assert_eq!(q.pop_audio(), Some(2));
        assert!(q.error().unwrap().contains("overflow"));
        assert!(q.audio(4).is_err());
        assert!(!q.video(4));
        assert_eq!(q.stats().peak_audio, 2);
    }
    #[test]
    fn independent_consumers_and_terminal_errors_do_not_share_state() {
        let slow = MediaQueue::new(1, 1);
        let fast = MediaQueue::new(1, 1);
        slow.video(0);
        slow.audio(0).unwrap();
        for i in 0..1000 {
            fast.video(i);
            fast.audio(i).unwrap();
            assert_eq!(fast.pop_video(), Some(i));
            assert_eq!(fast.pop_audio(), Some(i));
        }
        slow.fail("disk full".into());
        slow.fail("later".into());
        assert_eq!(slow.error(), Some("disk full"));
        assert_eq!(fast.error(), None);
        assert_eq!(fast.stats().dropped_video, 0);
    }
}
