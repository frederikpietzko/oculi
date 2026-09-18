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
        VideoSourceSelector::ApplicationProcess {
            process,
            display_id,
            label,
        } => format!(
            "Application — {label} · PID {} · Display {} · Session-only{}",
            process.pid,
            display_id.0,
            if process.session.is_nil() {
                " · Reselect"
            } else {
                ""
            }
        ),
    }
}

pub fn audio_title(source: &AudioSourceSelector) -> String {
    match source {
        AudioSourceSelector::None => "Choose an audio source…".into(),
        AudioSourceSelector::Microphone { label, .. } => format!("Input — {label}"),
        AudioSourceSelector::Application { label, .. } => format!("Application audio — {label}"),
        AudioSourceSelector::ApplicationProcess { process, label } => format!(
            "Application audio — {label} · PID {} · Session-only{}",
            process.pid,
            if process.session.is_nil() {
                " · Reselect"
            } else {
                ""
            }
        ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApplicationProcessId, DisplayId};

    #[test]
    fn application_process_titles_and_options_keep_restored_selection_distinct() {
        let process = ApplicationProcessId {
            pid: 42,
            start_time: [123, 456],
            session: uuid::Uuid::from_u128(1),
        };
        let video = VideoSourceSelector::ApplicationProcess {
            process,
            display_id: DisplayId(7),
            label: "IntelliJ IDEA (java)".into(),
        };
        let audio = AudioSourceSelector::ApplicationProcess {
            process,
            label: "IntelliJ IDEA (java)".into(),
        };
        assert_eq!(
            video_title(&video),
            "Application — IntelliJ IDEA (java) · PID 42 · Display 7 · Session-only"
        );
        assert_eq!(
            audio_title(&audio),
            "Application audio — IntelliJ IDEA (java) · PID 42 · Session-only"
        );
        let restored_video = serde_json::from_str(&serde_json::to_string(&video).unwrap()).unwrap();
        let restored_audio = serde_json::from_str(&serde_json::to_string(&audio).unwrap()).unwrap();
        assert!(video_title(&restored_video).ends_with(" · Reselect"));
        assert!(audio_title(&restored_audio).ends_with(" · Reselect"));
        assert_eq!(
            video_options(std::slice::from_ref(&video), &restored_video),
            vec![VideoSourceSelector::None, video.clone(), restored_video]
        );
        assert_eq!(
            audio_options(std::slice::from_ref(&audio), &restored_audio),
            vec![AudioSourceSelector::None, audio.clone(), restored_audio]
        );
        assert_eq!(video_options(std::slice::from_ref(&video), &video).len(), 2);
        assert_eq!(audio_options(std::slice::from_ref(&audio), &audio).len(), 2);
    }
}
