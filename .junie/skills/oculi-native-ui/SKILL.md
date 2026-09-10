---
name: oculi-native-ui
description: Change Oculi native AppKit workspace, dialogs, recording controls, Metal preview presentation and OBS-like single-source framing while preserving main-thread safety, coordinate geometry and UI smoke coverage.
---

# Oculi native UI and framing

Use for workspace/dialog behavior, blank startup windows, group controls, viewport
gestures, or displayed preview defects. Read [AGENTS.md](../../../AGENTS.md) and
use [oculi-macos-verification](../oculi-macos-verification/SKILL.md) for native checks.
Paths below are repository-relative.

## 1. Choose the smallest owning component

- `src/ui/mod.rs`: AppKit delegate, timers, discovery/persistence polling, window
  lifecycle, integrated native smoke phases.
- `workspace.rs`: left source drawer, central preview cards, bottom output drawer.
- `dialogs.rs` / `source_picker.rs`: source/audio drafts, global Settings, selectors.
- `group.rs` with `src/app.rs`: commands, configuration guards, status, quit flow.
- `viewport.rs`: Framing window, gestures, numeric controls, preview view/overlays.
- `src/render/transform.rs`: pure source/canvas geometry;
  `src/render/macos.rs`: clean GPU output and native presentation boundary.

Inspect the existing action and state guard before adding another selector or
window. This is AppKit through objc2, not a webview or a Swift companion app.

## 2. Preserve interaction and thread behavior

- Construct/mutate AppKit on the main thread with existing `MainThreadMarker` /
  `MainThreadOnly` conventions. Native capture, rendering, writes, and finalization
  remain asynchronous. Do not wait on GPU or filesystem operations in UI handlers.
- Keep semantic native colors, light/dark support, accessibility labels, resizing,
  and truncation. Reuse dedicated status labels; rebuilding cards every poll can
  destroy preview layers and flicker even when all state tests pass.
- Add/Edit uses a cancellable video/audio draft; Delete confirms configuration
  removal, not media deletion. Global settings remain a separate menu-bar window.
- Start/Pause/Resume/Stop operate on the group. Guard mutations in `AppState`,
  not just disabled buttons. Commit valid output drafts before Start and keep
  source/output locked during preparation, recording, pause, and finalization.
- Preserve immediate pending-Start locks and sequence acknowledgments. A stale
  Failed or Idle snapshot must not unlock a retry. Selection/framing remain usable.
- Quit must await Stop & Save asynchronously; report failure rather than claiming
  all files saved or terminating while writers still own accepted input.
- Workspace startup uses neutral first responder to avoid eagerly creating the
  macOS text-input helper window. Keep fields editable when explicitly focused.
  For a blank window report, inspect actual AppKit windows/focus/visibility first;
  do not hide arbitrary native helper windows or disable text input wholesale.

## 3. Separate geometry, rendering, and presentation

- The transform stores normalized source crop, canvas-relative translation, and
  scale relative to centered aspect-fit. Geometry uses top-left pixel coordinates;
  convert AppKit bottom-left points and backing scale explicitly, including letterbox.
- Output canvas dimensions stay fixed as a source resizes. Fit preserves crop;
  Fill scales to cover; Reset restores defaults. Validate finite/bounded crop,
  offsets and scale; invalid numeric edits must not poison subsequent rendering.
- Gestures and numeric changes target a stable recording ID; independent framings
  may share capture but never mutable transform state. Transform changes must not
  restart capture. Send snapshots at render boundaries and persist pending edits.
- Clean rendered output feeds both previews and writers. Handles are separate
  sibling overlays, never drawn into encoded surfaces. Preserve previews while
  paused, event-tracking updates while dragging, and visibility throttling.
- Native drawable presentation and raw CVPixelBuffer orientation are distinct
  boundaries. Preserve the presentation-only compensation in `render/macos.rs`;
  do not flip the entire renderer to fix an AppKit screenshot.
- Keep deferred GPU-resource retirement on close/delete and bounded submissions.
  Queueing a frame does not establish completed presentation.

## 4. Reproduce and verify at the appropriate layer

For state/geometry changes, start with relevant tests:

```sh
cargo test --lib app:: -- --nocapture
cargo test --lib ui:: -- --nocapture
cargo test --lib render:: -- --nocapture
```

For native changes, bundle and run the UI smoke using the verification skill.
Inspect isolated screenshots and actual assertions for:

- source draft save/cancel, deletion confirmation, invalid output, menu/dialog flow;
- only workspace visible on launch and no automatic text editing;
- pending Start locks and backend rejection (not fabricated recording success);
- Framing visible on an unlocked desktop, tracking-mode updates and GPU completion;
- quadrant orientation in clean framing, framing-with-handles, and card screenshots:
  synthetic red/green at the top, blue/white at the bottom;
- fixed-canvas edits, independent transforms, and handles absent from clean output.

Do not replace composited pixel assertions with raw-buffer checks: they previously
missed a native vertical inversion. Do not weaken the visible-window assertion
when the desktop is locked. Report blocked/skipped checks and request an unlocked
retest. An intermittent visual glitch remains unconfirmed unless reproduced;
distinguish a regression for its suspected cause from proof the symptom is gone.
