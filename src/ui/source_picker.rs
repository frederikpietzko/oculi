use crate::discovery::{same_audio, same_video};
use crate::model::{AudioSourceSelector, VideoSourceSelector};

pub fn video_title(source: &VideoSourceSelector) -> String {
    match source {
        VideoSourceSelector::None => "Choose a video source…".into(),
        VideoSourceSelector::Camera { label, .. } => format!("Camera — {label}"),
        VideoSourceSelector::Display { label, .. } => format!("Display — {label}"),
        VideoSourceSelector::Window { title, .. } => format!("Window — {title}"),
        VideoSourceSelector::Application {
            label, display_id, ..
        } => format!("Application — {label} · Display {}", display_id.0),
    }
}

pub fn audio_title(source: &AudioSourceSelector) -> String {
    match source {
        AudioSourceSelector::None => "Choose an audio source…".into(),
        AudioSourceSelector::Microphone { label, .. } => format!("Input — {label}"),
        AudioSourceSelector::Application { label, .. } => format!("Application audio — {label}"),
        AudioSourceSelector::System => "System audio — All playback".into(),
    }
}

pub fn video_options(
    available: &[VideoSourceSelector],
    selected: &VideoSourceSelector,
) -> Vec<VideoSourceSelector> {
    let mut options = vec![VideoSourceSelector::None];
    options.extend_from_slice(available);
    if !options.iter().any(|s| same_video(s, selected)) {
        options.push(selected.clone());
    }
    options
}

pub fn audio_options(
    available: &[AudioSourceSelector],
    selected: &AudioSourceSelector,
) -> Vec<AudioSourceSelector> {
    let mut options = vec![AudioSourceSelector::None];
    options.extend_from_slice(available);
    if !options.iter().any(|s| same_audio(s, selected)) {
        options.push(selected.clone());
    }
    options
}
