use super::*;
use crate::model::ViewportTransform;
use crate::recording::timeline::Timeline;
use crate::render::{Renderer, synthetic_sample};
use objc2_core_audio_types::{kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked};
use std::time::{Duration, Instant};

fn settings(width: u32, height: u32) -> OutputSettings {
    OutputSettings {
        resolution: Resolution { width, height },
        fps: 10,
        ..Default::default()
    }
}

/// Permission-independent PCM with CF-owned storage and no borrowed callbacks.
pub(super) fn synthetic_tone(frames: usize, planar: bool) -> NativeSample {
    unsafe {
        let mut asbd = AudioStreamBasicDescription {
            mSampleRate: 48_000.0,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat
                | kAudioFormatFlagIsPacked
                | if planar {
                    kAudioFormatFlagIsNonInterleaved
                } else {
                    0
                },
            mBytesPerPacket: if planar { 4 } else { 8 },
            mFramesPerPacket: 1,
            mBytesPerFrame: if planar { 4 } else { 8 },
            mChannelsPerFrame: 2,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        let mut raw_format = null();
        assert_eq!(
            CMAudioFormatDescriptionCreate(
                None,
                NonNull::from(&mut asbd),
                0,
                null(),
                0,
                null(),
                None,
                NonNull::from(&mut raw_format)
            ),
            0
        );
        let format = CFRetained::from_raw(NonNull::new(raw_format.cast_mut()).unwrap());
        let timing = CMSampleTimingInfo {
            duration: CMTime {
                value: 1,
                timescale: 48_000,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            },
            presentationTimeStamp: media_time(100.0).unwrap(),
            decodeTimeStamp: kCMTimeInvalid,
        };
        let size = asbd.mBytesPerFrame as usize;
        let mut raw = null_mut();
        assert_eq!(
            CMSampleBuffer::create_ready(
                None,
                None,
                Some(&format),
                frames as isize,
                1,
                &timing,
                if planar { 0 } else { 1 },
                if planar { null() } else { &size },
                NonNull::from(&mut raw)
            ),
            0
        );
        let sample = CFRetained::from_raw(NonNull::new(raw).unwrap());
        let tone = |index: usize| {
            ((index as f64 * 440.0 * std::f64::consts::TAU / 48_000.0).sin() * 0.2) as f32
        };
        let mut left: Vec<f32> = if planar {
            (0..frames).map(tone).collect()
        } else {
            (0..frames).flat_map(|i| [tone(i), -tone(i)]).collect()
        };
        let mut right: Vec<f32> = if planar {
            (0..frames).map(|i| -tone(i)).collect()
        } else {
            Vec::new()
        };
        let mut list = PcmBuffers {
            count: if planar { 2 } else { 1 },
            buffers: [
                AudioBuffer {
                    mNumberChannels: if planar { 1 } else { 2 },
                    mDataByteSize: (left.len() * 4) as u32,
                    mData: left.as_mut_ptr().cast(),
                },
                AudioBuffer {
                    mNumberChannels: 1,
                    mDataByteSize: (right.len() * 4) as u32,
                    mData: right.as_mut_ptr().cast(),
                },
            ],
        };
        assert_eq!(
            sample.set_data_buffer_from_audio_buffer_list(
                None,
                None,
                0,
                NonNull::from(&mut list).cast()
            ),
            0
        );
        NativeSample::retain(&sample, SampleKind::Audio).unwrap()
    }
}

fn retry(mut append: impl FnMut() -> Result<bool, String>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !append().unwrap() {
        assert!(
            Instant::now() < deadline,
            "native input backpressure timeout"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn finish(handle: &FinishHandle) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(result) = handle.poll() {
            result.unwrap();
            return;
        }
        assert!(Instant::now() < deadline, "native finalization timeout");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn host_clock_audio_drift_does_not_stop_recording() {
    let directory = tempfile::tempdir().unwrap();
    let output = settings(80, 40);
    let path = directory.path().join("clock-drift.mp4");
    let mut writer = Writer::prepare(&path, &output).unwrap();
    let pcm = synthetic_tone(1_024, false);
    let mut renderer = Renderer::new().unwrap();
    let frame = renderer
        .render(
            &synthetic_sample(80, 40).unwrap(),
            ViewportTransform::default(),
            output.resolution,
        )
        .unwrap();
    let origin = 123_456.25;
    let timeline = Timeline::new(origin).unwrap();
    let duration = 1_024.0 / 48_000.0;
    // A supported device clock runs 500 ppm faster than the host clock.
    // Each callback overlaps the nominal previous end by about 10.7 us,
    // despite containing consecutive, distinct PCM frames.
    let host_duration = duration * 0.9995;
    let buffers = 480;
    for index in 0..buffers {
        let timestamp = timeline.map(origin + index as f64 * host_duration).unwrap();
        retry(|| writer.append_video(&frame, timestamp));
        retry(|| writer.append_audio(&pcm, 0, 1_024, timestamp));
        assert!(writer.append_audio(&pcm, 0, 1_024, timestamp).is_err());
    }
    let end = buffers as f64 * host_duration;
    finish(&writer.finish(end).unwrap());
    let decoded = decode(&path, output.resolution, &[(10, 10)]);
    assert_eq!(decoded.pts.len(), buffers);
    assert!(decoded.audio_packets >= buffers);
    assert!(decoded.audio_pts.windows(2).all(|pair| pair[0] < pair[1]));
    // Compressed AAC includes encoder priming/padding. Check the playable track
    // range, not the padded final packet's end, against the group stop boundary.
    assert!((decoded.audio_track_end - end).abs() < 1.0 / 48_000.0);
    assert!((decoded.duration - end).abs() < 0.03);
}

#[test]
fn audio_clock_tolerance_is_bounded_and_never_accepts_reordered_buffers() {
    let rate = 48_000.0;
    for frames in [1, 512, 1_024, 4_800] {
        let previous = 0.25;
        let duration = frames as f64 / rate;
        let end = previous + duration;
        assert!(validate_audio_timestamp(end, previous, end, rate).is_ok());
        assert!(validate_audio_timestamp(end + 0.1, previous, end, rate).is_ok());
        assert!(validate_audio_timestamp(previous + duration * 0.999, previous, end, rate).is_ok());
        assert!(validate_audio_timestamp(previous, previous, end, rate).is_err());
        assert!(validate_audio_timestamp(previous - 0.01, previous, end, rate).is_err());
        assert!(
            validate_audio_timestamp(end - 2.0 / rate - duration * 0.001, previous, end, rate)
                .is_err()
        );
    }
}

#[derive(Debug)]
struct Decoded {
    pts: Vec<f64>,
    landmarks: Vec<Vec<[u8; 4]>>,
    audio_pts: Vec<f64>,
    audio_packets: usize,
    audio_end: f64,
    audio_track_end: f64,
    duration: f64,
}

#[allow(deprecated)]
fn decode(path: &Path, canvas: Resolution, points: &[(usize, usize)]) -> Decoded {
    autoreleasepool(|_| unsafe {
        let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str().unwrap()));
        let asset = AVURLAsset::URLAssetWithURL_options(&url, None);
        let videos = asset.tracksWithMediaType(AVMediaTypeVideo.unwrap());
        let audios = asset.tracksWithMediaType(AVMediaTypeAudio.unwrap());
        assert_eq!(videos.len(), 1);
        assert_eq!(audios.len(), 1);
        let video = videos.objectAtIndex(0);
        assert_eq!(video.naturalSize().width, canvas.width as f64);
        assert_eq!(video.naturalSize().height, canvas.height as f64);
        let matrix = video.preferredTransform();
        assert_eq!(
            (matrix.a, matrix.b, matrix.c, matrix.d),
            (1.0, 0.0, 0.0, 1.0)
        );
        let audio = audios.objectAtIndex(0);
        let reader = AVAssetReader::assetReaderWithAsset_error(&asset).unwrap();
        let key: &NSString = &*(kCVPixelBufferPixelFormatTypeKey as *const _ as *const NSString);
        let output_settings = dictionary(&[(key, &*NSNumber::new_u32(kCVPixelFormatType_32BGRA))]);
        let video_output = AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(
            &video,
            Some(&output_settings),
        );
        let audio_output =
            AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(&audio, None);
        assert!(reader.canAddOutput(&video_output));
        reader.addOutput(&video_output);
        assert!(reader.canAddOutput(&audio_output));
        reader.addOutput(&audio_output);
        assert!(reader.startReading());
        let mut result = Decoded {
            pts: Vec::new(),
            landmarks: Vec::new(),
            audio_pts: Vec::new(),
            audio_packets: 0,
            audio_end: 0.0,
            audio_track_end: audio.timeRange().start.seconds()
                + audio.timeRange().duration.seconds(),
            duration: asset.duration().seconds(),
        };
        while let Some(sample) = video_output.copyNextSampleBuffer() {
            result.pts.push(sample.presentation_time_stamp().seconds());
            let image = sample.image_buffer().unwrap();
            assert_eq!(
                CVPixelBufferGetPixelFormatType(&image),
                kCVPixelFormatType_32BGRA
            );
            assert_eq!(CVPixelBufferGetWidth(&image), canvas.width as usize);
            assert_eq!(CVPixelBufferGetHeight(&image), canvas.height as usize);
            assert_eq!(
                CVPixelBufferLockBaseAddress(&image, CVPixelBufferLockFlags::ReadOnly),
                0
            );
            let base = CVPixelBufferGetBaseAddress(&image).cast::<u8>();
            let stride = CVPixelBufferGetBytesPerRow(&image);
            result.landmarks.push(
                points
                    .iter()
                    .map(|&(x, y)| {
                        std::ptr::read_unaligned(base.add(y * stride + x * 4).cast::<[u8; 4]>())
                    })
                    .collect(),
            );
            assert_eq!(
                CVPixelBufferUnlockBaseAddress(&image, CVPixelBufferLockFlags::ReadOnly),
                0
            );
        }
        while let Some(sample) = audio_output.copyNextSampleBuffer() {
            // AVAssetReader also emits zero-sample boundary markers. They are
            // not AAC packets; verify their empty payload rather than decoding.
            if sample.num_samples() == 0 {
                assert_eq!(sample.total_sample_size(), 0);
                assert!(sample.format_description().is_none());
                continue;
            }
            let format = sample.format_description().unwrap();
            assert_eq!(format.media_sub_type(), kAudioFormatMPEG4AAC);
            assert!(sample.num_samples() > 0);
            assert!(sample.total_sample_size() > 0);
            result.audio_packets += sample.num_samples() as usize;
            let start = sample.presentation_time_stamp().seconds();
            if !result.audio_pts.is_empty() {
                assert!((start - result.audio_end).abs() < 1.0 / 48_000.0);
            }
            result.audio_end = start + sample.duration().seconds();
            result
                .audio_pts
                .push(sample.presentation_time_stamp().seconds());
        }
        assert_eq!(
            reader.status(),
            AVAssetReaderStatus::Completed,
            "{:?}",
            reader.error()
        );
        assert!(result.pts.windows(2).all(|pair| pair[1] > pair[0]));
        assert!(result.audio_pts.windows(2).all(|pair| pair[1] > pair[0]));
        assert!(!result.audio_pts.is_empty());
        result
    })
}

fn near_color(actual: [u8; 4], expected: [u8; 4]) {
    for channel in 0..3 {
        assert!(
            actual[channel].abs_diff(expected[channel]) < 35,
            "pixel {actual:?}, expected {expected:?}"
        );
    }
}

#[test]
fn simultaneous_native_mp4s_keep_group_timing_and_independent_live_crops() {
    autoreleasepool(|_| {
        let directory = tempfile::tempdir().unwrap();
        let paths = [
            directory.path().join("a.mp4"),
            directory.path().join("b.mp4"),
        ];
        let outputs = [settings(80, 80), settings(160, 80)];
        let mut writers = [
            Writer::prepare(&paths[0], &outputs[0]).unwrap(),
            Writer::prepare(&paths[1], &outputs[1]).unwrap(),
        ];
        let mut renderers = [Renderer::new().unwrap(), Renderer::new().unwrap()];
        let source = synthetic_sample(80, 40).unwrap();
        let pcm = [synthetic_tone(67_200, false), synthetic_tone(67_200, true)];
        let mut timeline = Timeline::new(100.0).unwrap();
        timeline.pause(100.4).unwrap();
        timeline.resume(100.8).unwrap();
        timeline.stop(101.2).unwrap();
        // One callback overlaps group start, the entire pause, and group stop.
        let audio_spans = timeline.audio_spans(99.9, 67_200, 48_000).unwrap();
        assert_eq!(audio_spans.len(), 2);
        assert_eq!(
            (audio_spans[0].offset, audio_spans[0].frames),
            (4_800, 19_200)
        );
        assert_eq!(
            (audio_spans[1].offset, audio_spans[1].frames),
            (43_200, 19_200)
        );
        for tick in 0..8 {
            let host = 100.0 + tick as f64 / 10.0 + if tick >= 4 { 0.4 } else { 0.0 };
            let timestamp = timeline.map(host).unwrap();
            for index in 0..2 {
                // B arrives late, which must remain a group-relative empty edit.
                if index == 1 && tick == 0 {
                    continue;
                }
                let transform = if index == 0 {
                    ViewportTransform::default()
                } else {
                    ViewportTransform {
                        crop_x: if tick < 4 { 0.0 } else { 0.5 },
                        crop_width: 0.5,
                        crop_height: 0.5,
                        ..Default::default()
                    }
                };
                let frame = renderers[index]
                    .render(&source, transform, outputs[index].resolution)
                    .unwrap();
                retry(|| writers[index].append_video(&frame, timestamp));
                assert!(writers[index].append_video(&frame, timestamp).is_err());
                let span = &audio_spans[tick / 4];
                let offset = span.offset + (tick % 4) * 4_800;
                let audio_time = span.timestamp + (tick % 4) as f64 / 10.0;
                retry(|| writers[index].append_audio(&pcm[index], offset, 4_800, audio_time));
                assert!(
                    writers[index]
                        .append_audio(&pcm[index], offset, 4_800, audio_time)
                        .is_err()
                );
            }
        }
        assert!(writers[0].finish(0.6).is_err());
        let handles = [
            writers[0].finish(0.8).unwrap(),
            writers[1].finish(0.8).unwrap(),
        ];
        // Completion owns every native lifetime, even after the worker drops Writer.
        drop(writers);
        for handle in &handles {
            finish(handle);
        }
        let a = decode(
            &paths[0],
            outputs[0].resolution,
            &[(10, 5), (10, 30), (70, 30), (10, 50), (70, 50)],
        );
        let b = decode(&paths[1], outputs[1].resolution, &[(80, 40)]);
        assert_eq!(a.pts.len(), 8);
        // The reader materializes B's initial empty edit as a black frame at
        // zero, then presents its first real frame at the original group PTS.
        assert_eq!(b.pts.len(), 8, "{b:?}");
        assert!((a.pts[0] - 0.0).abs() < 0.001);
        assert_eq!(b.pts[0], 0.0);
        near_color(b.landmarks[0][0], [0, 0, 0, 255]);
        assert!((b.pts[1] - 0.1).abs() < 0.001, "{b:?}");
        for result in [&a, &b] {
            assert!((result.duration - 0.8).abs() < 0.04, "{result:?}");
            assert!((result.pts.last().unwrap() - 0.7).abs() < 0.001);
            assert!(result.audio_pts.last().unwrap() < &0.81);
        }
        for landmarks in &a.landmarks {
            for (actual, expected) in landmarks.iter().zip([
                [0, 0, 0, 255],
                [0, 0, 255, 255],
                [0, 255, 0, 255],
                [255, 0, 0, 255],
                [255, 255, 255, 255],
            ]) {
                near_color(*actual, expected);
            }
        }
        for (pts, landmarks) in b.pts.iter().zip(&b.landmarks) {
            near_color(
                landmarks[0],
                if *pts == 0.0 {
                    [0, 0, 0, 255]
                } else if *pts < 0.4 {
                    [0, 0, 255, 255]
                } else {
                    [0, 255, 0, 255]
                },
            );
        }
    });
}

#[test]
fn bounded_encoder_workers_save_concurrently_with_static_frame_cadence() {
    use crate::recording::{
        encoder::{Encoder, EncoderState},
        timeline::FrameCadence,
    };
    use crate::storage::{naming::TemplateContext, reservation::OutputReservation};
    let directory = tempfile::tempdir().unwrap();
    let output = OutputSettings {
        fps: 30,
        ..settings(160, 120)
    };
    let context = TemplateContext {
        name: "concurrent".into(),
        timestamp: time::OffsetDateTime::now_utc(),
        counter: 1,
    };
    let mut encoders: Vec<_> = (0..2)
        .map(|_| {
            Encoder::prepare(
                output.clone(),
                OutputReservation::reserve(directory.path(), "{name}_{counter}.mp4", &context)
                    .unwrap(),
            )
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(10);
    while encoders
        .iter()
        .any(|e| matches!(e.state(), EncoderState::Preparing))
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    for encoder in &encoders {
        assert!(
            matches!(encoder.state(), EncoderState::Ready),
            "{:?}",
            encoder.state()
        );
    }
    let mut renderer = Renderer::new().unwrap();
    let start = Instant::now();
    let frame = renderer
        .render(
            &crate::render::synthetic_sample(80, 40).unwrap(),
            ViewportTransform::default(),
            output.resolution,
        )
        .unwrap();
    let render_latency = start.elapsed();
    let tone = synthetic_tone(1600, false);
    let mut cadence = FrameCadence::new(30).unwrap();
    for n in 0..15 {
        let timestamp = cadence.due(n as f64 / 30.0).unwrap();
        for encoder in &mut encoders {
            encoder.video(frame.clone(), timestamp);
            encoder.audio(tone.clone(), 0, 1600, timestamp).unwrap();
        }
        std::thread::sleep(Duration::from_millis(34));
    }
    for encoder in &mut encoders {
        encoder.finish(0.5);
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while encoders
        .iter()
        .any(|e| matches!(e.state(), EncoderState::Ready | EncoderState::Finalizing))
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    for encoder in encoders {
        let stats = encoder.stats();
        assert!(stats.peak_video <= 2 && stats.peak_audio <= 128);
        eprintln!(
            "Synthetic encoder: {stats:?}, render latency {render_latency:?}, error {:?}",
            encoder.error()
        );
        assert!(encoder.error().is_none());
        let EncoderState::Saved(path) = encoder.state() else {
            panic!("{:?}", encoder.state());
        };
        let decoded = decode(&path, output.resolution, &[(40, 40)]);
        assert!(!decoded.pts.is_empty());
        assert!((decoded.duration - 0.5).abs() < 0.05, "{decoded:?}");
        near_color(decoded.landmarks[0][0], [0, 0, 255, 255]);
    }
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
}

#[test]
fn pcm_range_copy_retimes_without_mutating_source_or_host_metadata() {
    for planar in [false, true] {
        let original = synthetic_tone(4_800, planar);
        let sample = original.clone().with_host_timestamp(200.0).unwrap();
        assert_eq!(original.timestamp_seconds(), 100.0);
        assert_eq!(sample.timestamp_seconds(), 200.0);
        for invalid in [f64::NAN, f64::INFINITY, -1.0] {
            assert!(sample.clone().with_host_timestamp(invalid).is_err());
        }
        unsafe {
            let asbd = pcm_description(&sample, 123, 456).unwrap();
            let copied = copy_audio(&sample, 123, 456, media_time(0.25).unwrap(), asbd).unwrap();
            assert_eq!(copied.num_samples(), 456);
            assert_eq!(copied.presentation_time_stamp().seconds(), 0.25);
            assert!((copied.duration().seconds() - 456.0 / 48_000.0).abs() < 1e-9);
            assert_eq!(
                sample.buffer_read().presentation_time_stamp().seconds(),
                100.0
            );
            let mut values = [vec![0f32; if planar { 456 } else { 912 }], vec![0f32; 456]];
            let mut list = PcmBuffers {
                count: if planar { 2 } else { 1 },
                buffers: std::array::from_fn(|i| AudioBuffer {
                    mNumberChannels: if planar { 1 } else { 2 },
                    mDataByteSize: (values[i].len() * 4) as u32,
                    mData: values[i].as_mut_ptr().cast(),
                }),
            };
            assert_eq!(
                copied.copy_pcm_data_into_audio_buffer_list(
                    0,
                    456,
                    NonNull::from(&mut list).cast()
                ),
                0
            );
            for i in 0..456 {
                let expected = (((i + 123) as f64 * 440.0 * std::f64::consts::TAU / 48_000.0).sin()
                    * 0.2) as f32;
                assert_eq!(values[0][if planar { i } else { 2 * i }], expected);
                assert_eq!(
                    if planar {
                        values[1][i]
                    } else {
                        values[0][2 * i + 1]
                    },
                    -expected
                );
            }
        }
    }
}

#[test]
fn invalid_settings_destinations_samples_and_native_backpressure_are_explicit() {
    autoreleasepool(|_| {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid.mp4");
        for output in [
            settings(0, 80),
            settings(81, 80),
            settings(u32::MAX - 1, 80),
            OutputSettings {
                fps: 0,
                ..settings(80, 80)
            },
        ] {
            assert!(Writer::prepare(&path, &output).is_err());
            assert!(!path.exists());
        }
        let existing = directory.path().join("existing.mp4");
        std::fs::write(&existing, b"do not replace").unwrap();
        assert!(Writer::prepare(&existing, &settings(80, 80)).is_err());
        assert_eq!(std::fs::read(&existing).unwrap(), b"do not replace");
        assert!(
            Writer::prepare(
                &directory.path().join("missing/child.mp4"),
                &settings(80, 80)
            )
            .is_err()
        );
        let mut writer = Writer::prepare(&path, &settings(80, 80)).unwrap();
        let pcm = synthetic_tone(4_800, false);
        let source = synthetic_sample(80, 40).unwrap();
        let mut renderer = Renderer::new().unwrap();
        let frame = renderer
            .render(
                &source,
                Default::default(),
                Resolution {
                    width: 80,
                    height: 80,
                },
            )
            .unwrap();
        assert!(writer.append_audio(&source, 0, 1, 0.0).is_err());
        for (offset, frames) in [(0, 0), (4_799, 2), (usize::MAX, 1)] {
            assert!(writer.append_audio(&pcm, offset, frames, 0.0).is_err());
        }
        for timestamp in [f64::NAN, f64::INFINITY, -0.1] {
            assert!(writer.append_video(&frame, timestamp).is_err());
            assert!(writer.append_audio(&pcm, 0, 10, timestamp).is_err());
            assert!(writer.finish(timestamp).is_err());
        }
        unsafe {
            let native = writer.native.as_ref().unwrap();
            assert!(native.video.expectsMediaDataInRealTime());
            assert!(native.audio.expectsMediaDataInRealTime());
            // A real native not-ready input, not a mocked readiness result.
            native.video.markAsFinished();
            native.audio.markAsFinished();
        }
        assert!(!writer.append_video(&frame, 0.0).unwrap());
        assert!(!writer.append_audio(&pcm, 0, 480, 0.0).unwrap());
        assert_eq!(writer.last_video, None);
        assert_eq!(writer.last_audio, None);
        assert_eq!(writer.audio_end, None);
        writer.cancel();
        writer.cancel();
        assert!(writer.append_video(&frame, 0.0).is_err());
        assert!(writer.append_audio(&pcm, 0, 480, 0.0).is_err());
        assert!(writer.finish(1.0).is_err());
        assert!(!path.exists());
    });
}
