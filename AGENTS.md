# Oculi agent onboarding

## Start here

Oculi is a Rust 2024, native AppKit application for **macOS 15+**. It records
multiple independently framed video/audio pairs into separate H.264/AAC MP4s.
Capture, live previews, framing, encoding, and group controls are implemented;
do not treat the old hello-world description in the design plan as current code.

- Read [README.md](README.md) for user behavior, build commands, and smoke checks.
- Inspect the current working tree before editing; preserve unrelated user work.
- Consult the [delivery plan](.junie/plans/native-macos-multi-recorder.md) when doing
  planned feature work. Do not reopen completed steps for unrelated fixes/docs.
  Completed steps are not evidence that hardware checks passed on today's build.
- Discover unfamiliar code semantically with `jbcontext search` when available;
  once a path/symbol is known, read it directly or use targeted search.
- Keep changes focused, follow surrounding Rust/objc2 conventions, and add a
  failing regression before fixing behavioral bugs where feasible.

## Junie skills

This root `AGENTS.md` is the shared onboarding entry point. Project skills use
Junie's native `.junie/skills/<name>/SKILL.md` layout, not another agent's config
directory. Each has YAML `name` and `description` metadata for discovery.

Read the matching skill before doing that work:

| Skill | Use for |
| --- | --- |
| [oculi-macos-verification](.junie/skills/oculi-macos-verification/SKILL.md) | Build, bundle, TCC permissions, hardware/UI smoke checks, interpreting evidence |
| [oculi-media-pipeline](.junie/skills/oculi-media-pipeline/SKILL.md) | Capture sharing, queues, timestamps, MP4 writers, safe saves and shutdown |
| [oculi-native-ui](.junie/skills/oculi-native-ui/SKILL.md) | AppKit workspace/dialogs, group control locking, Metal previews and framing |

Junie selects enabled skills by relevance; they can also be requested explicitly
by name (for example, `$oculi-macos-verification`). Keep these skills procedural;
update them when interfaces or verification commands change, not with session logs.

## Code map

| Area | Entry points and responsibilities |
| --- | --- |
| Launch / state | `src/main.rs`: app and smoke CLI; `src/lib.rs`: module wiring; `src/app.rs`: settings, drafts, selection, configuration locks |
| Domain / discovery | `src/model.rs`: selectors/configuration; `src/discovery.rs`: identity and availability without source substitution |
| Native UI | `src/ui/mod.rs`: delegate, polling, smoke flow; `workspace.rs`: sidebar/cards/output drawer; `dialogs.rs`: source/settings windows; `group.rs`: recording actions; `viewport.rs`: framing and presentation |
| Capture core | `src/capture/registry.rs`: shared leases, bounded delivery; `service.rs`: demand reconciliation; `mod.rs`: requests/formats/errors |
| Native capture | `src/platform/macos/`: permissions/discovery, ScreenCaptureKit, AVFoundation devices, immutable samples; `capture.rs`: worker bridge; `preview.rs`: render scheduling |
| Rendering | `src/render/transform.rs`: pure geometry; `macos.rs`: GPU renderer, pixel pools, native presentation |
| Recording | `src/recording/coordinator.rs`: lifecycle; `runtime.rs`: media routing/preflight; `timeline.rs`/`clock.rs`: shared timing; `encoder.rs`/`queue.rs`: bounded workers; `writer.rs`: AVAssetWriter |
| Storage | `src/storage/settings.rs`: versioned atomic persistence; `naming.rs`: templates; `reservation.rs`: staging and no-replacement publication |
| Packaging | `resources/Info.plist`, `scripts/bundle.sh`; defaults to `target/debug/Oculi.app` |

Most tests are inline. Real encode/decode coverage also lives in
`src/recording/writer_tests.rs`; do not assume the `tests/` directory is the suite.

## Product and safety invariants

- Exactly one required video and one required audio source per recording; audio
  selection is independent of video. No mixing, scenes, monitoring, or streaming.
- Application video is display-scoped; application audio is app-wide, not
  window-specific. Persisted window IDs require reselection; never silently
  replace an unavailable target or infer identity from its display label.
- Compatible sources share capture, not transforms or encoded files. Framing
  remains editable during recording/pauses; source/output settings are locked.
- Start/Pause/Resume/Stop are group-wide with one logical host-clock timeline.
  Fatal source/writer errors stop the group and attempt usable partial saves.
- AppKit stays on the main thread. Capture callbacks never wait for rendering,
  disk I/O, UI, or slow subscribers. Bounded video loss has diagnostics; audio
  overflow is an explicit error, not silent data loss.
- Never add blanket `Send`/`Sync` for native objects. Preserve concrete immutable
  sample/frame audits and owners until asynchronous readers/GPU work complete.
- Preview and writer consume the same clean per-recording render; editor handles
  are separate. Retina presentation must not alter output canvas dimensions.
- Never overwrite recordings. Keep reservation/staging ownership and atomic
  no-replacement promotion; retain and report failed artifacts explicitly.
- Do not change bundle identity/signing or reset permissions as a casual fix.
  Do not edit real settings at `~/Library/Application Support/Oculi/settings.json`
  or capture private desktop/device content without task authorization.

## Development and evidence

For code changes, start with relevant module tests, then broaden proportionally:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -- --nocapture
```

Normal tests include real Metal/AVFoundation synthetic media checks on macOS;
they are not a substitute for authorized live-device tests. For docs-only changes,
check links/metadata/diffs without rebuilding or launching capture.

Use the verification skill before bundling or running native smoke. Rebuilding
the ad-hoc signed app can lose TCC authorization; a test executable has a different
identity. Report commands, PASS/FAIL/SKIP, and artifact paths, not historical test
counts. A passing test that printed SKIP did **not** verify the hardware.

For parallel development, give each subagent exclusive files and explicit APIs;
join before integration, full tests, or rebuilding the bundle another check uses.
Keep generated artifacts under `target/`, never in `.junie/`. Do not delete user
media, saved settings, plan files, or artifacts you did not create. Do not commit
unless asked. Release signing/notarization and `/demo` setup remain separate work;
do not assume a Linux demo VM can run this native macOS app.
