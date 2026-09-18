# Oculi

A native Rust/AppKit multi-recording app for macOS 15+.

For project development, see [AGENTS.md](AGENTS.md) for the code map, safety
contracts, and Junie skills for native UI, media pipelines, and macOS verification.

## Current implementation

The native workspace shows every configured video/audio pair as a preview card,
with a source sidebar and a collapsible recording-output drawer. Add/edit dialogs,
delete confirmation, source discovery, destination settings and persistence work.
Shared native video/audio capture runs on a background worker, with per-recording
status and bounded sample delivery. Live Metal previews and independent single-source
framing are implemented. Independent H.264/AAC MP4 writers share one group timeline,
with bounded queues, pause trimming, and collision-safe output publication.
Group controls coordinate preflight, recording, and asynchronous Stop & Save.

## Build and launch

```sh
bash scripts/bundle.sh
open target/debug/Oculi.app
```

Use `bash scripts/bundle.sh release` for an optimized build. Bundles are locally
ad-hoc signed, not notarized. The script sets the macOS 15 deployment target.
`Cargo.lock` records the resolved dependencies. No FFmpeg installation is needed.

For Apple Silicon direct-download releases, see [production build and signing](docs/releases.md).
`bash scripts/release.sh local` builds a local-only optimized app without a paid
Apple account. `bash scripts/release.sh publish` requires Developer ID credentials
and produces a signed, notarized, stapled ZIP; it never falls back to ad-hoc signing.

## Configuration

- Use **Add Source…** in the left sidebar to name a recording and choose its video
  and audio in a native configuration sheet. **Edit…** opens the same sheet;
  **Cancel** discards the draft. **Duplicate** creates a separate configuration,
  and **Delete** asks for confirmation without deleting any saved media files.
- Select a sidebar row or preview card to edit its output folder, filename,
  resolution and FPS in the bottom **Recording output** drawer. **Apply Output**
  saves changes; Return, switching sources and quitting also apply valid output
  edits. Collapse the drawer to make more room for the card grid.
- The resolution dropdown offers discovered source sizes and common presets, plus
  **Custom…** for manual width/height entry. New recordings default to the highest
  discovered resolution of the **first configured video source**, even when later
  sources have different sizes. Highest means largest pixel area; odd native sizes
  are rounded down to even H.264 dimensions. Until discovery succeeds, the fallback
  is 1920×1080. Defaults resolve once; saved sizes and explicit overrides are preserved,
  and resizing a source does not resize an already resolved output canvas.
  Presets are output sizes, not guarantees of camera FPS or hardware encoder support;
  native writer support is checked before recording.
- Preview cards show live, independently framed video. **Framing…** opens a larger
  editor: drag to reposition, drag a corner to resize, Option-drag a corner to crop,
  and scroll to zoom. Numeric controls plus Fit / Fill / Reset are also available.
  Crop is normalized to the source; offsets are fractions of the fixed output canvas.
  Framing changes are saved without restarting capture. Handles are separate native
  overlays and never part of the clean rendered surface.
- **Start** preflights every configured pair before committing a common start.
  **Pause / Resume** omit paused time while previews keep running. **Stop & Save**
  finishes every output asynchronously. These controls always operate on the group.
  Source/output settings are locked while active; selection and framing remain editable.
  A required source or writer failure stops the group and attempts partial finalization.
- Available configured sources begin capture automatically while Oculi is open;
  macOS may show camera, microphone, or screen-capture indicators. Media is only
  written after Start; there is no audible monitoring. Delete an idle configuration to release its subscriptions;
  a shared source stops when its last subscriber leaves. Quitting stops all sources.
- The sidebar and output drawer show capture status. **Refresh Sources** explicitly
  retries failed captures; periodic discovery does not continuously retry a busy
  device. Check Oculi Settings for permissions and teardown errors.
- If ScreenCaptureKit times out or a native stop fails, that source is quarantined
  until Oculi restarts. This avoids opening another stream while cleanup may still
  be pending. Native cleanup cannot be forced to complete within a fixed deadline.
- Compatible recordings share source capture, not encoded output. The largest
  requested video dimensions/FPS are negotiated first; raising them can restart
  the shared device. Unsupported device formats are rejected rather than silently
  substituting another device. Playback audio is independent of the video filter.
- Diagnostic video is latest-only, so skipped-sample counters include deliberate
  replacement of older frames. Audio queues are bounded; overflow is an explicit
  capture error. Atomic subscriber snapshots and bounded concurrent queues keep
  routine source refresh, cleanup, and polling from dropping audio on mutex contention.
  Writer queues separately report late video drops and fatal audio overflow.
  Per-recording status includes activity, queue depths, and saved paths or failures.
- Rendering runs separately from capture/audio draining, with latest-only handoffs,
  bounded pixel-buffer pools, and at most two in-flight submissions per native preview.
  Active previews render at up to 30 FPS, inactive cards at up to 5 FPS, and hidden
  previews suspend rendering when not recording. Active recordings render at their
  configured cadence even offscreen and reuse static frames. Framing changes re-render; source
  errors clear stale previews. Retina scaling affects presentation, not output dimensions.
- Open **Oculi → Settings…** (`⌘,`) for global permissions and source discovery.
  Settings opens in its own window; appearance follows the macOS light/dark mode.
- Video categories include cameras, displays, windows, and applications on each
  display. Audio categories include input devices, application playback, and
  system playback. Selecting application audio does not restrict it to one window.
  Application video remains selectable when macOS marks its shareable windows as
  off-screen, using their window bounds to identify displays. Listing a source
  does not guarantee hidden/minimized windows or windows on another Space appear
  in the recording; bring the desired UI onto the selected display to capture it.
- Microphone/input-device audio is captured as mono and centered in both channels
  of the stereo AAC recording. Application and system playback retain stereo.
  Bounded device timestamp jitter is tolerated without shifting audio away from
  the group clock. Duplicate/backward buffers, excessive overlap, and sustained
  clock error still stop recording rather than silently discarding audio.
- Sources refresh in the background, with device-change notifications and periodic
  screen-content refresh. Unavailable sources stay selected; no substitute is chosen.
- Saved windows require explicit reselection after restarting because window IDs
  can be recycled. Multiple instances with the same app bundle ID are not offered
  as an ambiguous application target.
- Applications without a usable bundle ID can appear as **Session-only** process
  targets, showing their process label and PID (and display for video). Reselect
  both video and audio after restarting Oculi or the source process; saved process
  targets are never silently matched to a reused PID. Application video can include
  shareable popups belonging to the same process on the selected display; separate
  helper applications are not automatically included. Process audio is independent
  of the selected display and is not window-specific.
  Executable paths can improve labels (for example, `IntelliJ IDEA (java)`), but
  do not group separate PIDs: matching executable paths or labels never merges
  different processes into one capture target.
- Grant permissions using the explicit **Allow** buttons. If access was denied,
  use System Settings → Privacy & Security → Camera, Microphone, or Screen & System
  Audio Recording. macOS may require restarting the app after permission changes.
  Background refresh does not intentionally request permission.
- Settings live in `~/Library/Application Support/Oculi/settings.json`. Invalid or
  unsupported settings are preserved and editing is disabled until repaired.
- Templates accept `{name}`, `{date}`, `{time}`, and `{counter}`; output is MP4.
  One group timestamp is used for naming. Reservations and atomic no-replacement
  promotion prevent overwriting existing files; counter suffixes resolve collisions.
  Destination checks run off the UI thread. Failed media remains explicitly incomplete
  in a hidden `.oculi-incomplete-*` destination subdirectory, not a promised recovered file.
  Destinations must support same-filesystem hard links for final publication.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -- --nocapture
bash scripts/bundle.sh
target/debug/Oculi.app/Contents/MacOS/oculi --smoke-test target/smoke
```

The UI smoke run creates isolated settings/output directories beneath the supplied
path and exits after exercising source-sheet save/cancel, output validation,
delete confirmation/cancellation, drawer resizing, the Settings menu and
asynchronous persistence. It also writes `workspace.png`, `workspace-light.png`
and `workspace-dark.png` layout snapshots. Its synthetic GPU phase exercises the
actual display link, event tracking, independent framing and fixed-canvas changes.
With screen-recording permission, composited `workspace-live.png`, `framing.png`
and `framing-clean.png` snapshots also verify displayed output separately from handles.
It never uses the regular settings file. Native discovery smoke is opt-in via
`OCULI_NATIVE_DISCOVERY_SMOKE=1 cargo test native_discovery_smoke -- --nocapture`;
it reports skips if the required permissions are missing.

For application-list diagnosis without starting capture or requiring camera/microphone
access, run `OCULI_APPLICATION_DISCOVERY_BUNDLE=com.jetbrains.intellij cargo test
native_application_discovery_smoke -- --nocapture`. It checks the actual catalog for
that running application's video entry; missing screen permission is a SKIP.

Capture adapter smoke tests are opt-in:

```sh
OCULI_NATIVE_CAPTURE_SMOKE=1 cargo test platform::macos:: -- --nocapture --test-threads=1
```

Camera/microphone permissions granted to Oculi may not apply to the separate test
executable. To check real samples under the bundled app identity instead:

```sh
bash scripts/bundle.sh
mkdir -p target/smoke
open -n -W target/debug/Oculi.app --args --device-smoke-test "$PWD/target/smoke/devices-$(uuidgen).txt"
```

The new report file contains the bundle identity, each device's PASS/FAIL/SKIP,
and a completion marker. This explicit check does not open the workspace, change
saved configurations, prompt for permissions, or save media. It checks the default
camera at 640×480, 1920×1080, and 1000×560, then the microphone, stopping each
session. Camera reports include native input dimensions separately from the
validated output dimensions. Cameras without input-priority support retain
automatic native input negotiation; AVFoundation's output conversion scales to
the requested dimensions with aspect fit, rather than requiring that sensor
format. This does not increase the camera's native frame rate; the writer repeats
the latest frame at the output cadence. A missing permission
is still a skip, not successful verification; rebuilding an ad-hoc signed app may
require macOS to grant access again.

These require existing macOS capture permissions and suitable live sources; they
never request permission automatically. Missing prerequisites are reported as
skips, not hardware verification. Application/system audio needs active playback.
Real audio routing and device disconnection behavior still require a permissioned
macOS session; unit tests use fake adapters to exercise sharing, failures and queues.
Permission-independent tests encode and decode simultaneous MP4s through the real
renderer and writer, checking AAC tracks, dimensions, crop landmarks, timestamps,
pause trimming, bounded queues, safe saves, and group lifecycle failure scenarios.

GPU geometry/pixel tests run in the normal test suite without capture permissions.
The shared-preview smoke additionally exercises live capture, two independently framed
outputs, output resizing, and capture/render shutdown:

```sh
OCULI_NATIVE_CAPTURE_SMOKE=1 cargo test native_shared_preview_smoke -- --nocapture
open -n -W target/debug/Oculi.app --args --preview-smoke-test "$PWD/target/smoke/previews-$(uuidgen).txt"
```

The preview check defaults to a display. For a camera, run the bundled executable
with `OCULI_PREVIEW_SOURCE=camera`; it still requires that app's existing permission.
Preview report exit codes are 0 for success, 1 for failure, and 2 for missing prerequisites.

The explicit full-group recording smoke captures a display with system audio into
two separate files, pauses/resumes, changes framing, saves, and tears down capture:

```sh
open -n -W target/debug/Oculi.app --args --recording-smoke-test "$PWD/target/smoke/recordings-$(uuidgen).txt"
```

It uses isolated output locations, does not change saved configurations or prompt
for permissions, and reports paths and diagnostics. System audio needs active playback.
This check creates media files intentionally; inspect the report for PASS, FAIL, or
SKIP rather than treating missing hardware/permissions as successful verification.
