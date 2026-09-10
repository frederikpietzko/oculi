# Direct-download releases

Oculi releases target **Apple Silicon, macOS 15+**, using a local Mac. The bundle
identifier remains `dev.oculi.recorder`. This is not an App Store workflow.

## Without Apple Developer membership

You can build an optimized app now, but cannot create a Developer ID-signed,
Apple-notarized release without paid [Apple Developer Program membership](https://developer.apple.com/programs/).
A free Apple account or self-signed certificate is not a substitute. Ad-hoc builds
are for local testing; recipients may encounter Gatekeeper blocks.

```sh
rustup target add aarch64-apple-darwin
bash scripts/release.sh local
```

The script prints a fresh `Oculi.app` path under Cargo's target directory in
`releases/Oculi-<version>-<build>-local.<suffix>/`. It enables hardened runtime
and the release entitlements, but does not notarize or create a distribution ZIP.
It never replaces `target/debug/Oculi.app` or `target/release/Oculi.app`.

## One-time signing setup after enrollment

1. Install Apple's Command Line Tools (or Xcode), Rust, and the ARM target above.
   Confirm `xcrun --find notarytool` and `xcrun --find stapler` succeed.
2. In Keychain Access, use Certificate Assistant to create a certificate signing
   request. In [Certificates, Identifiers & Profiles](https://developer.apple.com/account/resources/certificates/list),
   create a **Developer ID Application** certificate using that request, download
   it, and install it in the same Keychain as its private key. Your account needs
   permission to create this certificate; ask the team Account Holder if needed.
   A Developer ID Installer certificate is not needed for ZIP distribution.
3. Verify the identity is valid and has its private key:

   ```sh
   security find-identity -v -p codesigning
   ```

4. Create an app-specific password for your Apple Account and store notarization
   credentials in Keychain. Run this **interactively in your own Terminal**;
   enter your Apple Account, Team ID, and app-specific password when prompted:

   ```sh
   xcrun notarytool store-credentials "oculi-notary"
   ```

Do not put passwords, private keys, or exported certificates in the repository,
scripts, chat, or shell history. Keep a secure backup of your signing key. The
release script uses your existing unlocked Keychain; macOS may ask you to approve
access to the signing key on first use. It does not change Keychain permissions.

## Build a distributable release

Set the numeric `major.minor.patch` version in `Cargo.toml` (and update
`Cargo.lock` if needed). The release script derives the bundled version from Cargo,
not the development plist. Choose an increasing build number for each publication.

```sh
export OCULI_SIGNING_IDENTITY='Developer ID Application: Your Name (TEAMID)'
export OCULI_NOTARY_PROFILE='oculi-notary'
export OCULI_BUILD_NUMBER=1
bash scripts/release.sh publish
```

`publish` is the default and fails rather than falling back to ad-hoc signing.
It builds with `--locked --release --target aarch64-apple-darwin`, verifies ARM64,
signs with hardened runtime and a secure timestamp, uploads to Apple's notary
service, requires `Accepted`, staples and validates the ticket, checks the
signature and Gatekeeper, then creates the final ZIP and SHA-256 checksum.
Network access to Apple is required. No app is launched and no capture is started.

Each run keeps its own directory under `target/releases/` (or your configured
Cargo target directory), including signature details, entitlements, the submission
ZIP, and `notary-result.json`. Distribute **only** the final
`Oculi-<version>-<build>-macos-arm64.zip` and its `.sha256` file, **not**
`notary-submission.zip`, which was created before stapling. Failed runs retain
artifacts for diagnosis and must not be distributed.

For rejection or a timeout, use the submission `id` in `notary-result.json`:

```sh
xcrun notarytool info '<submission-id>' --keychain-profile oculi-notary
xcrun notarytool log '<submission-id>' --keychain-profile oculi-notary
```

A timeout does not cancel Apple's processing. Resolve the problem and rerun the
release script to complete all checks in a fresh directory; do not treat an upload
or command exit alone as notarization approval.

## Before publishing

- Run `bash scripts/test-release.sh` for packaging preflight regression checks;
  these do not build, sign, submit, or launch the app.
- Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo test -- --nocapture` against the release source.
- Test the **final signed app** on a permissioned Mac using the smoke procedures
  in [README.md](../README.md#verification), replacing the example debug bundle
  paths with this release's path. Authorize capture explicitly; do not reset TCC
  or alter real settings just to pass a test. Signing changes may require renewed
  permission. No hardware check is implied by a successful build/notarization.
- Test the downloaded final ZIP's extraction and launch with Gatekeeper enabled
  on a separate Mac, including the minimum supported macOS version. Confirm
  camera, microphone, screen/system audio, Metal preview, and MP4 recording work.

`resources/Release.entitlements` grants camera and audio-input access under
hardened runtime. It does not enable App Sandbox, disable library validation,
grant debugging/JIT exceptions, or bypass user-granted capture permissions.
See Apple's [notarization requirements](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution).