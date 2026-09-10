# Oculi

A native Rust/AppKit multi-recording app for macOS 15+.

## Current implementation

The native workspace shows every configured video/audio pair as a preview card,
with a source sidebar and a collapsible recording-output drawer. Add/edit dialogs,
delete confirmation, source discovery, destination settings and persistence work.
Shared native video/audio capture runs on a background worker, with per-recording
status and bounded sample delivery. **Live previews, viewport editing, media encoding
and group recording controls are not implemented yet. This build does not record files.**

## Build and launch

```sh
bash scripts/bundle.sh
open target/debug/Oculi.app
```

Use `bash scripts/bundle.sh release` for an optimized build. Bundles are locally
ad-hoc signed, not notarized. The script sets the macOS 15 deployment target.
`Cargo.lock` records the resolved dependencies. No FFmpeg installation is needed.

## Configuration

- Use **Add Source…** in the left sidebar to name a recording and choose its video
  and audio in a native configuration sheet. **Edit…** opens the same sheet;
  **Cancel** discards the draft. **Duplicate** creates a separate configuration,
  and **Delete** asks for confirmation without deleting any saved media files.
- Select a sidebar row or preview card to edit its output folder, filename,
  resolution and FPS in the bottom **Recording output** drawer. **Apply Output**
  saves changes; Return, switching sources and quitting also apply valid output
  edits. Collapse the drawer to make more room for the card grid.
- Preview cards are labeled placeholders, not live video. The group-wide
  **Start / Pause / Stop & Save** buttons remain disabled until encoding and
  recording are implemented. They are not individual recording controls.
- Available configured sources begin capture automatically while Oculi is open;
  macOS may show camera, microphone, or screen-capture indicators. No media is
  written or played back. Delete a configuration to release its subscriptions;
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
  capture error. Writer subscriptions and recording overload handling come later.
- Open **Oculi → Settings…** (`⌘,`) for global permissions and source discovery.
  Settings opens in its own window; appearance follows the macOS light/dark mode.
- Video categories include cameras, displays, windows, and applications on each
  display. Audio categories include input devices, application playback, and
  system playback. Selecting application audio does not restrict it to one window.
- Sources refresh in the background, with device-change notifications and periodic
  screen-content refresh. Unavailable sources stay selected; no substitute is chosen.
- Saved windows require explicit reselection after restarting because window IDs
  can be recycled. Multiple instances with the same app bundle ID are not offered
  as an ambiguous application target.
- Grant permissions using the explicit **Allow** buttons. If access was denied,
  use System Settings → Privacy & Security → Camera, Microphone, or Screen & System
  Audio Recording. macOS may require restarting the app after permission changes.
  Background refresh does not intentionally request permission.
- Settings live in `~/Library/Application Support/Oculi/settings.json`. Invalid or
  unsupported settings are preserved and editing is disabled until repaired.
- Templates accept `{name}`, `{date}`, `{time}`, and `{counter}`; output is MP4.
  Destination checks run off the UI thread. Save errors remain visible for retry.

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
and `workspace-dark.png` layout snapshots.
It never uses the regular settings file. Native discovery smoke is opt-in via
`OCULI_NATIVE_DISCOVERY_SMOKE=1 cargo test native_discovery_smoke -- --nocapture`;
it reports skips if the required permissions are missing.

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
camera and microphone sequentially and stops each session. A missing permission
is still a skip, not successful verification; rebuilding an ad-hoc signed app may
require macOS to grant access again.

These require existing macOS capture permissions and suitable live sources; they
never request permission automatically. Missing prerequisites are reported as
skips, not hardware verification. Application/system audio needs active playback.
Real audio routing and device disconnection behavior still require a permissioned
macOS session; unit tests use fake adapters to exercise sharing, failures and queues.
No encoding or saved-file verification is included at this stage.