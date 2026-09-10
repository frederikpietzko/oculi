# Oculi

A native Rust/AppKit multi-recording app for macOS 15+.

## Current implementation

The native workspace shows every configured video/audio pair as a preview card,
with a source sidebar and a collapsible recording-output drawer. Add/edit dialogs,
delete confirmation, source discovery, destination settings and persistence work.
**Capture, previews, viewport editing, media encoding and group recording controls
are not implemented yet. This build does not record files.**

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
  **Start / Pause / Stop & Save** buttons remain disabled until capture and
  recording are implemented. They are not individual recording controls.
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
it reports skips if the required permissions are missing. It does not test capture
or encoding, which are later delivery stages.