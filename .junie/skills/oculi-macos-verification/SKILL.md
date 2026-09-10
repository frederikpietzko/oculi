---
name: oculi-macos-verification
description: Build and verify Oculi on macOS, run bundled UI, camera, microphone, preview and recording smoke checks, and diagnose TCC permissions without confusing skips with passes.
---

# Oculi macOS verification

Use this for build/test requests, native smoke failures, or permission-sensitive
retests. Read [AGENTS.md](../../../AGENTS.md) and the verification section in
[README.md](../../../README.md). Run commands from the repository root.

## 1. Select the evidence needed before rebuilding

- Check the current tree and whether the task authorizes live capture or media
  creation. Normal app launch auto-captures configured available sources.
- Identify the exact build under test. If the user just granted permissions,
  **test the existing bundle first**; do not rebuild/re-sign it unnecessarily.
- Check macOS 15+, device/source availability, and an unlocked interactive
  desktop for visible UI checks. App/system-audio checks need active playback.
- Never automate granting TCC permission, change the privacy database, or reset
  access as part of a test. Ask the user to authorize the exact app when needed.
- Do not run competing hardware/UI checks concurrently or replace a bundle
  while a smoke test is running. Do not kill unrelated app instances.

## 2. Run proportionate non-capture checks

For changed code, choose a module filter first, for example:

```sh
cargo test --lib recording::timeline -- --nocapture
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -- --nocapture
```

Default tests include real synthetic GPU/MP4 encoding and decoding on macOS.
Live-adapter tests are gated and may print SKIP even when Cargo exits successfully.
Docs-only work does not need Cargo or a new bundle.

## 3. Bundle only when the task needs a new executable

```sh
bash scripts/bundle.sh
```

The script uses `--locked`, sets `MACOSX_DEPLOYMENT_TARGET=15.0`, packages the
plist, and **ad-hoc signs** the bundle. `release` is its optional profile argument.
Use the path it prints if Cargo's target directory is customized; commands below
assume the default. The bundle identifier is `dev.oculi.recorder`.

Rebuilds may require authorization again. If so, ask the user to open this bundle,
grant access in Oculi Settings, and quit; retest without another rebuild. Permission
for the bundle does not prove permission for a `cargo test` executable.

## 4. Choose a smoke check

Create the report parent, and use a fresh report filename for each run; report
creation intentionally refuses overwrite:

```sh
mkdir -p target/smoke
```

| Need | Command (run only the needed check) |
| --- | --- |
| Native workspace, dialogs, group controls, synthetic displayed previews | `target/debug/Oculi.app/Contents/MacOS/oculi --smoke-test target/smoke` |
| Camera format/scaling and microphone samples under app identity | `open -n -W target/debug/Oculi.app --args --device-smoke-test "$PWD/target/smoke/devices-$(uuidgen).txt"` |
| Live shared display previews and independent transforms | `open -n -W target/debug/Oculi.app --args --preview-smoke-test "$PWD/target/smoke/previews-$(uuidgen).txt"` |
| Two real display/system-audio MP4s, pause/resume, framing, finalization | `open -n -W target/debug/Oculi.app --args --recording-smoke-test "$PWD/target/smoke/recordings-$(uuidgen).txt"` |

For camera previews, set the environment on the bundled executable directly;
do not assume LaunchServices forwards shell variables:

```sh
OCULI_PREVIEW_SOURCE=camera target/debug/Oculi.app/Contents/MacOS/oculi --preview-smoke-test "$PWD/target/smoke/camera-preview-$(uuidgen).txt"
```

These helpers avoid normal saved settings and do not prompt for permissions.
Recording smoke intentionally creates media and reports its paths. UI smoke uses
a unique isolated directory and can capture screenshots; keep artifacts private.
Visible/composited checks require an unlocked desktop and screenshot permission.

For explicit adapter/discovery work, the separate test executable can be used
when it has its own required authorization:

```sh
OCULI_NATIVE_DISCOVERY_SMOKE=1 cargo test native_discovery_smoke -- --nocapture
OCULI_NATIVE_CAPTURE_SMOKE=1 cargo test platform::macos:: -- --nocapture --test-threads=1
```

## 5. Interpret failures without weakening checks

- Read the new report, bundle identity, per-check results, and final
  `COMPLETE exit_code=...`; `open` succeeding alone is not test success.
  Report codes are `0` success, `1` failure, `2` missing prerequisites.
- Missing permission/device is **SKIP / not verified**, not PASS. A missing
  completion marker means an incomplete/crashed run, not success.
- A locked desktop can leave Framing occluded despite a healthy renderer.
  Request an unlocked retest or report it blocked; keep visibility assertions.
- Distinguish raw pixel tests, native displayed orientation, and real capture.
  One passing does not establish the other two.
- A shared-preview run observing zero sessions is unresolved until investigated;
  successful recording or UI smoke does not erase a different failed check.
- Preserve error reports and incomplete media. Do not delete arbitrary staging
  directories or user settings to obtain a clean-looking run.

Finish with exact checks/results, artifact paths, bundle rebuilt or reused, and
remaining prerequisites. Never carry forward an old hardware PASS as new evidence.
