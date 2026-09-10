---
name: oculi-media-pipeline
description: Safely change Oculi shared capture, native sample ownership, bounded media queues, audio clock timing, group recording lifecycle, AVAssetWriter output and collision-safe persistence.
---

# Oculi media pipeline changes

Use for capture, overload, timing, encoding, saving, or shutdown work. Read
[AGENTS.md](../../../AGENTS.md); use
[oculi-macos-verification](../oculi-macos-verification/SKILL.md) for live validation.
Paths below are repository-relative.

## Locate and reproduce the failing boundary

1. Start from the exact error or affected source, not a rewrite of the pipeline.
   Trace one recording ID/source key through `src/capture/{registry,service}.rs`,
   `src/platform/macos/{capture,preview}.rs`, and `src/recording/runtime.rs` as needed.
2. Identify whether loss occurs at the native callback, registry subscription,
   render handoff, runtime inbox, encoder queue, or AVAssetWriter readiness.
   Retain the original error and queue/timestamp diagnostics.
3. Add the smallest reproducer: fake-backend tests for sharing/lifecycle, synthetic
   clocks/PCM for retiming, or real synthetic render/encode/decode for output.
   Demonstrate failure before fixing; permission-gated skips are not reproductions.

## Preserve these contracts

### Capture and ownership

- Source identity is typed; labels are not identity. Never substitute a camera,
  app instance, recycled window, or audio source to make preflight succeed.
- `registry.rs` shares compatible capture via leases; native teardown occurs on
  the owning worker after the last lease is released. Retired publishers must not
  affect replacement sessions. Failed shared sources fan out to every dependent.
- `screen_capture.rs` selects video and playback audio independently. App audio
  is app-wide; app video is on the selected display. Preserve own-playback exclusion.
- `device_capture.rs` supports automatic input negotiation when input-priority
  configuration is unavailable. Output scaling is separate from sensor format;
  never claim scaling increases a camera's native FPS.
- `NativeSample` in `sample.rs` and `RenderedFrame` in `render/macos.rs` have
  concrete immutable ownership audits. Shared CF references can still expose
  mutators: do not turn unsafe buffer readers into safe raw accessors or permit
  aliases to escape without their owner. No generic native `Send`/`Sync` wrappers.
- Preserve timeout quarantine after uncertain native startup/stop. A caller
  timeout does not prove the native stream stopped; avoid repeated retry workers.

### Queues and rendering

- Capture callbacks must not wait on subscription membership, preview, disk, or
  writer readiness. Preserve bounded concurrent delivery and sticky failure paths.
- Video can drop/replace frames with counters; audio cannot be silently dropped.
  Fix false contention errors without removing real overflow detection or
  replacing bounded queues with unbounded ones.
- `preview.rs` and runtime share completed per-recording rendered frames, not
  separate compositions. Hidden recordings still render at output cadence;
  static screen content can reuse frames, but explicit source loss must fail.
- Pixel pools and GPU submissions remain bounded. Retain every surface owner
  through asynchronous readers; GPU waits must not migrate onto AppKit/callbacks.

### Timing and lifecycle

- `clock.rs` converts device timestamps into host time. `timeline.rs` maps a
  single committed group start and pause history for both tracks. Never zero
  each track independently, splice files after pauses, or use callback arrival
  time as media time.
- Trim PCM at start/pause/resume/stop boundaries; preserve original buffer
  immutability. Cadence emits current slots rather than an unbounded catch-up burst.
- `writer.rs::validate_audio_timestamp` allows bounded nominal-duration vs host
  clock drift while rejecting duplicate/backward/excessive overlap. Do not restore
  the old 1 ns non-overlap rule or snap every buffer to nominal end (accumulating
  A/V drift). Extend drift and negative tests if changing this policy.
- AVAssetWriter backpressure `false` is not accepted media: video counts a drop;
  audio must be retried within a bound or fail. Do not advance accepted timing
  state when an append was refused.
- `coordinator.rs` drives preflight/rollback/finalization; runtime acknowledges
  commands only after processing. `GroupSnapshot.command_sequence` prevents an
  old Failed/Idle snapshot from unlocking a newer pending Start in `app.rs`.
- Stop closes admission, drains accepted input, and awaits all usable writers.
  Quit/drop must finalize before capture teardown, not abandon files or block UI.

### Files and settings

- `storage/reservation.rs` owns exclusive markers/staging and uses same-filesystem
  hard links for atomic no-replacement publication. A filename proposal in
  `naming.rs` alone is not an atomic reservation. Never replace promotion with a
  potentially overwriting rename or delete another run's marker/artifacts.
- Publish only after successful writer completion. Failed media remains explicitly
  incomplete; rollback may remove only owned empty preparation artifacts.
- `settings.rs` uses versioned atomic saves. Preserve invalid/unsupported files
  and require deliberate migration rather than silently overwriting them.

## Validate the change

Run relevant filters, not every live source by default:

```sh
cargo test --lib capture:: -- --nocapture
cargo test --lib recording::timeline -- --nocapture
cargo test --lib recording::writer -- --nocapture
cargo test --lib recording::runtime -- --nocapture
cargo test --lib recording::coordinator -- --nocapture
cargo test --lib storage:: -- --nocapture
```

Use `writer_tests.rs` and runtime synthetic tests for concurrent real MP4 output:
verify tracks, dimensions, monotonic timestamps, pause duration, decoded crop
landmarks, and independent live transforms. Keep negative cases for overflow,
failed preflight/rollback, repeated commands, stop while paused, save failure,
and shutdown. Then run formatting/lint/full tests for cross-cutting changes.

Report which layer changed and which ownership/timing bounds remain true. Native
delivery smoke alone does not prove audio-routing isolation or long-term A/V sync.
