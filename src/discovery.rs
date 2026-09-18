//! Platform-independent source identity and validation.

use crate::model::{
    AudioSourceSelector, RecordingConfig, Resolution, SourceAvailability, VideoSourceSelector,
};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SourceCatalog {
    pub videos: Vec<VideoSourceSelector>,
    pub audios: Vec<AudioSourceSelector>,
    pub video_resolutions: Vec<(VideoSourceSelector, Vec<Resolution>)>,
    pub warnings: Vec<String>,
}

impl SourceCatalog {
    /// Native sizes, largest pixel area first, rounded down for H.264's even dimensions.
    pub fn resolutions(&self, source: &VideoSourceSelector) -> Vec<Resolution> {
        if matches!(source, VideoSourceSelector::None)
            || !self.videos.iter().any(|video| same_video(video, source))
        {
            return Vec::new();
        }
        let mut sizes: Vec<_> = self
            .video_resolutions
            .iter()
            .filter(|(video, _)| same_video(video, source))
            .flat_map(|(_, sizes)| sizes.iter())
            .filter_map(|size| {
                let size = Resolution {
                    width: size.width / 2 * 2,
                    height: size.height / 2 * 2,
                };
                (size.width > 0 && size.height > 0).then_some(size)
            })
            .collect();
        sort_resolutions(&mut sizes);
        sizes
    }
    /// Updates availability only; never changes a selection or its saved label.
    /// `None` is unavailable: a required source has not been selected.
    ///
    /// Window IDs are valid only within this process session. The caller MUST
    /// keep restored window selections unavailable after this call until the
    /// user explicitly reselects a window, even if its numeric ID matches.
    /// Restored application processes have nil sessions and cannot revalidate.
    pub fn revalidate(&self, config: &mut RecordingConfig) {
        config.video_availability = availability(
            !matches!(config.video, VideoSourceSelector::None)
                && !matches!(&config.video, VideoSourceSelector::ApplicationProcess { process, .. } if process.session.is_nil())
                && self.videos.iter().any(|v| same_video(v, &config.video)),
        );
        config.audio_availability = availability(
            !matches!(config.audio, AudioSourceSelector::None)
                && !matches!(&config.audio, AudioSourceSelector::ApplicationProcess { process, .. } if process.session.is_nil())
                && self.audios.iter().any(|a| same_audio(a, &config.audio)),
        );
    }

    /// Identity-only, order-independent comparison, useful for hotplug diffs.
    /// Use `PartialEq` instead when label or warning changes should update UI.
    pub fn same_sources(&self, other: &Self) -> bool {
        self.videos
            .iter()
            .all(|a| other.videos.iter().any(|b| same_video(a, b)))
            && other
                .videos
                .iter()
                .all(|a| self.videos.iter().any(|b| same_video(a, b)))
            && self
                .audios
                .iter()
                .all(|a| other.audios.iter().any(|b| same_audio(a, b)))
            && other
                .audios
                .iter()
                .all(|a| self.audios.iter().any(|b| same_audio(a, b)))
    }
}

pub fn sort_resolutions(sizes: &mut Vec<Resolution>) {
    sizes.sort_by_key(|size| {
        std::cmp::Reverse((
            u64::from(size.width) * u64::from(size.height),
            size.width,
            size.height,
        ))
    });
    sizes.dedup();
}

fn availability(found: bool) -> SourceAvailability {
    if found {
        SourceAvailability::Available
    } else {
        SourceAvailability::Unavailable
    }
}

pub fn same_video(a: &VideoSourceSelector, b: &VideoSourceSelector) -> bool {
    use VideoSourceSelector::*;
    match (a, b) {
        (None, None) => true,
        (Camera { device_id: a, .. }, Camera { device_id: b, .. }) => a == b,
        (Display { display_id: a, .. }, Display { display_id: b, .. }) => a == b,
        (
            Window {
                window_id: a,
                owner_bundle_id: ao,
                ..
            },
            Window {
                window_id: b,
                owner_bundle_id: bo,
                ..
            },
        ) => a == b && ao == bo,
        (
            Application {
                bundle_id: a,
                display_id: ad,
                ..
            },
            Application {
                bundle_id: b,
                display_id: bd,
                ..
            },
        ) => a == b && ad == bd,
        (
            ApplicationProcess {
                process: a,
                display_id: ad,
                ..
            },
            ApplicationProcess {
                process: b,
                display_id: bd,
                ..
            },
        ) => a == b && ad == bd,
        _ => false,
    }
}

pub fn same_audio(a: &AudioSourceSelector, b: &AudioSourceSelector) -> bool {
    use AudioSourceSelector::*;
    match (a, b) {
        (None, None) | (System, System) => true,
        (Microphone { device_id: a, .. }, Microphone { device_id: b, .. }) => a == b,
        (Application { bundle_id: a, .. }, Application { bundle_id: b, .. }) => a == b,
        (ApplicationProcess { process: a, .. }, ApplicationProcess { process: b, .. }) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApplicationProcessId, BundleId, DeviceId, DisplayId, WindowId};

    #[test]
    fn application_process_identity_uses_pid_start_time_session_and_video_display() {
        let process = ApplicationProcessId {
            pid: 42,
            start_time: [123, 456],
            session: uuid::Uuid::from_u128(1),
        };
        let video = |process, display, label: &str| VideoSourceSelector::ApplicationProcess {
            process,
            display_id: DisplayId(display),
            label: label.into(),
        };
        let audio = |process, label: &str| AudioSourceSelector::ApplicationProcess {
            process,
            label: label.into(),
        };
        assert!(same_video(
            &video(process, 1, "Old"),
            &video(process, 1, "New")
        ));
        assert!(same_audio(&audio(process, "Old"), &audio(process, "New")));
        assert!(!same_video(
            &video(process, 1, "App"),
            &video(process, 2, "App")
        ));
        for different in [
            ApplicationProcessId { pid: 43, ..process },
            ApplicationProcessId {
                start_time: [124, 456],
                ..process
            },
            ApplicationProcessId {
                start_time: [123, 457],
                ..process
            },
            ApplicationProcessId {
                session: uuid::Uuid::from_u128(2),
                ..process
            },
            ApplicationProcessId {
                session: uuid::Uuid::nil(),
                ..process
            },
        ] {
            assert!(!same_video(
                &video(process, 1, "App"),
                &video(different, 1, "App")
            ));
            assert!(!same_audio(
                &audio(process, "App"),
                &audio(different, "App")
            ));
        }
        let mut config = RecordingConfig::new("Process");
        config.video = video(process, 2, "Saved");
        config.audio = audio(process, "Saved");
        SourceCatalog {
            videos: vec![video(process, 1, "Renamed")],
            audios: vec![audio(process, "Renamed")],
            ..Default::default()
        }
        .revalidate(&mut config);
        assert_eq!(config.video_availability, SourceAvailability::Unavailable);
        assert_eq!(config.audio_availability, SourceAvailability::Available);
        assert_eq!(config.video, video(process, 2, "Saved"));
        assert_eq!(config.audio, audio(process, "Saved"));
    }

    #[test]
    fn nil_application_process_is_unavailable_even_if_in_catalog() {
        let process = ApplicationProcessId {
            pid: 42,
            start_time: [123, 456],
            session: uuid::Uuid::nil(),
        };
        let mut config = RecordingConfig::new("Restored");
        config.video = VideoSourceSelector::ApplicationProcess {
            process,
            display_id: DisplayId(1),
            label: "App".into(),
        };
        config.audio = AudioSourceSelector::ApplicationProcess {
            process,
            label: "App".into(),
        };
        SourceCatalog {
            videos: vec![config.video.clone()],
            audios: vec![config.audio.clone()],
            ..Default::default()
        }
        .revalidate(&mut config);
        assert_eq!(config.video_availability, SourceAvailability::Unavailable);
        assert_eq!(config.audio_availability, SourceAvailability::Unavailable);
    }

    fn camera(id: &str, label: &str) -> VideoSourceSelector {
        VideoSourceSelector::Camera {
            device_id: DeviceId(id.into()),
            label: label.into(),
        }
    }

    fn microphone(id: &str, label: &str) -> AudioSourceSelector {
        AudioSourceSelector::Microphone {
            device_id: DeviceId(id.into()),
            label: label.into(),
        }
    }

    #[test]
    fn resolution_choices_use_identity_and_rank_real_even_sizes_by_area() {
        let source = camera("a", "Same label");
        let mut catalog = SourceCatalog {
            videos: vec![source.clone()],
            video_resolutions: vec![
                (
                    camera("other", "Same label"),
                    vec![Resolution {
                        width: 7680,
                        height: 4320,
                    }],
                ),
                (
                    camera("a", "Old label"),
                    vec![
                        Resolution {
                            width: 1920,
                            height: 1080,
                        },
                        Resolution {
                            width: 3841,
                            height: 2161,
                        },
                        Resolution {
                            width: 3840,
                            height: 2160,
                        },
                        Resolution {
                            width: 4096,
                            height: 1000,
                        },
                        Resolution {
                            width: 0,
                            height: 1080,
                        },
                        Resolution {
                            width: 1,
                            height: 1,
                        },
                    ],
                ),
            ],
            ..Default::default()
        };
        assert_eq!(
            catalog.resolutions(&source),
            vec![
                Resolution {
                    width: 3840,
                    height: 2160
                },
                Resolution {
                    width: 4096,
                    height: 1000
                },
                Resolution {
                    width: 1920,
                    height: 1080
                },
            ]
        );
        assert!(catalog.resolutions(&VideoSourceSelector::None).is_empty());
        catalog.videos.clear();
        assert!(catalog.resolutions(&source).is_empty());
    }

    #[test]
    fn device_identity_ignores_labels_not_ids() {
        assert!(same_video(&camera("a", "Old"), &camera("a", "New")));
        assert!(!same_video(&camera("a", "Same"), &camera("b", "Same")));
        assert!(same_audio(&microphone("a", "Old"), &microphone("a", "New")));
        assert!(!same_audio(
            &microphone("a", "Same"),
            &microphone("b", "Same")
        ));
        assert!(!same_video(&VideoSourceSelector::None, &camera("a", "A")));
        assert!(!same_audio(
            &AudioSourceSelector::None,
            &AudioSourceSelector::System
        ));
    }

    #[test]
    fn display_window_and_application_identity_are_scoped() {
        let display = |id, label: &str| VideoSourceSelector::Display {
            display_id: DisplayId(id),
            label: label.into(),
        };
        assert!(same_video(&display(1, "Old"), &display(1, "New")));
        assert!(!same_video(&display(1, "Same"), &display(2, "Same")));
        let window = |id, owner: Option<&str>, title: &str| VideoSourceSelector::Window {
            window_id: WindowId(id),
            owner_bundle_id: owner.map(|s| BundleId(s.into())),
            title: title.into(),
        };
        assert!(same_video(
            &window(1, Some("a"), "Old"),
            &window(1, Some("a"), "New")
        ));
        assert!(!same_video(
            &window(1, Some("a"), ""),
            &window(2, Some("a"), "")
        ));
        assert!(!same_video(
            &window(1, Some("a"), ""),
            &window(1, Some("b"), "")
        ));
        assert!(!same_video(&window(1, Some("a"), ""), &window(1, None, "")));
        let app = |bundle: &str, display, label: &str| VideoSourceSelector::Application {
            bundle_id: BundleId(bundle.into()),
            display_id: DisplayId(display),
            label: label.into(),
        };
        assert!(same_video(&app("a", 1, "Old"), &app("a", 1, "New")));
        assert!(!same_video(&app("a", 1, ""), &app("a", 2, "")));
        assert!(!same_video(&app("a", 1, ""), &app("b", 1, "")));
        let audio = |bundle: &str, label: &str| AudioSourceSelector::Application {
            bundle_id: BundleId(bundle.into()),
            label: label.into(),
        };
        assert!(same_audio(&audio("a", "Old"), &audio("a", "New")));
        assert!(!same_audio(&audio("a", "Same"), &audio("b", "Same")));
    }

    #[test]
    fn revalidation_never_substitutes_and_audio_is_independent() {
        let mut config = RecordingConfig::new("Test");
        config.video = camera("missing", "Saved camera");
        config.audio = AudioSourceSelector::System;
        let saved = config.clone();
        let catalog = SourceCatalog {
            videos: vec![camera("different", "Saved camera")],
            audios: vec![AudioSourceSelector::System],
            warnings: vec![],
            ..Default::default()
        };
        catalog.revalidate(&mut config);
        assert_eq!(config.video_availability, SourceAvailability::Unavailable);
        assert_eq!(config.audio_availability, SourceAvailability::Available);
        config.video_availability = saved.video_availability;
        config.audio_availability = saved.audio_availability;
        assert_eq!(config, saved);
    }

    #[test]
    fn disconnect_and_reconnect_restore_availability_without_relabeling() {
        let mut config = RecordingConfig::new("Test");
        config.video = camera("a", "Saved");
        config.audio = microphone("m", "Saved");
        SourceCatalog::default().revalidate(&mut config);
        assert_eq!(config.video_availability, SourceAvailability::Unavailable);
        assert_eq!(config.audio_availability, SourceAvailability::Unavailable);
        SourceCatalog {
            videos: vec![camera("a", "Renamed")],
            audios: vec![microphone("m", "Renamed")],
            warnings: vec![],
            ..Default::default()
        }
        .revalidate(&mut config);
        assert_eq!(config.video_availability, SourceAvailability::Available);
        assert_eq!(config.audio_availability, SourceAvailability::Available);
        assert_eq!(config.video, camera("a", "Saved"));
        assert_eq!(config.audio, microphone("m", "Saved"));
    }

    #[test]
    fn none_is_unavailable_even_if_in_catalog() {
        let mut config = RecordingConfig::new("Test");
        SourceCatalog::default().revalidate(&mut config);
        assert_eq!(config.video_availability, SourceAvailability::Unavailable);
        assert_eq!(config.audio_availability, SourceAvailability::Unavailable);
        SourceCatalog {
            videos: vec![VideoSourceSelector::None],
            audios: vec![AudioSourceSelector::None],
            warnings: vec![],
            ..Default::default()
        }
        .revalidate(&mut config);
        assert_eq!(config.video_availability, SourceAvailability::Unavailable);
        assert_eq!(config.audio_availability, SourceAvailability::Unavailable);
    }

    #[test]
    fn catalog_diff_detects_hotplug_but_ignores_order_labels_and_warnings() {
        let a = SourceCatalog {
            videos: vec![camera("a", "Old"), camera("b", "B")],
            audios: vec![AudioSourceSelector::System],
            warnings: vec![],
            ..Default::default()
        };
        let mut b = SourceCatalog {
            videos: vec![camera("b", "B"), camera("a", "New")],
            audios: a.audios.clone(),
            warnings: vec!["Warning".into()],
            ..Default::default()
        };
        assert!(a.same_sources(&b));
        assert_ne!(a, b);
        b.videos.pop();
        assert!(!a.same_sources(&b));
        assert!(!b.same_sources(&a));
        b = a.clone();
        b.audios.clear();
        assert!(!a.same_sources(&b));
    }
}
