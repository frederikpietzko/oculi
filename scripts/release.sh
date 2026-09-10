#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
    printf 'Usage: bash scripts/release.sh [local|publish]\n'
    printf '  local: optimized, ad-hoc signed Apple Silicon app; NOT for distribution\n'
    printf '  publish (default): Developer ID sign, notarize, staple, verify, and ZIP\n'
    printf '  publish requires OCULI_SIGNING_IDENTITY, OCULI_NOTARY_PROFILE, OCULI_BUILD_NUMBER\n'
}

fail() {
    printf 'Error: %s\n' "$*" >&2
    exit 1
}

mode="${1:-publish}"
if [[ $# -gt 1 ]]; then
    usage >&2
    exit 2
fi
case "$mode" in
    local|publish) ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
esac

[[ "$(uname -s)" == Darwin ]] || fail 'Releases must be built on macOS.'
if [[ "$mode" == publish ]]; then
    [[ -n "${OCULI_SIGNING_IDENTITY:-}" ]] || fail 'Set OCULI_SIGNING_IDENTITY to your Developer ID Application certificate name. Paid Apple Developer membership is required; use local for an ad-hoc test build.'
    case "$OCULI_SIGNING_IDENTITY" in
        'Developer ID Application: '*) ;;
        *) fail 'Use a Developer ID Application identity, not Apple Development, Mac Distribution, or ad-hoc signing.' ;;
    esac
    [[ -n "${OCULI_NOTARY_PROFILE:-}" ]] || fail 'Set OCULI_NOTARY_PROFILE to a notarytool Keychain profile.'
    [[ -n "${OCULI_BUILD_NUMBER:-}" ]] || fail 'Set OCULI_BUILD_NUMBER to an increasing positive integer.'
    identities="$(/usr/bin/security find-identity -v -p codesigning)"
    [[ "$identities" == *"\"$OCULI_SIGNING_IDENTITY\""* ]] || fail 'The requested valid signing identity and private key are not available in Keychain.'
    xcrun --find notarytool >/dev/null
    xcrun --find stapler >/dev/null
fi

build_number="${OCULI_BUILD_NUMBER:-1}"
[[ "$build_number" =~ ^[1-9][0-9]*$ ]] || fail 'OCULI_BUILD_NUMBER must be a positive integer.'
export MACOSX_DEPLOYMENT_TARGET=15.0
target=aarch64-apple-darwin
metadata="$(cargo metadata --locked --no-deps --format-version 1)"
target_dir="$(/usr/bin/plutil -extract target_directory raw -o - - <<< "$metadata")"
version="$(/usr/bin/plutil -extract packages.0.version raw -o - - <<< "$metadata")"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail 'Use a numeric major.minor.patch Cargo version for a release.'

/usr/bin/plutil -lint resources/Info.plist resources/Release.entitlements
cargo build --locked --release --bin oculi --target "$target"
binary="$target_dir/$target/release/oculi"
[[ "$(/usr/bin/lipo -archs "$binary")" == arm64 ]] || fail 'Expected an Apple Silicon-only executable.'

mkdir -p "$target_dir/releases"
release_dir="$(mktemp -d "$target_dir/releases/Oculi-$version-$build_number-$mode.XXXXXX")"
trap 'printf "Release failed; retained artifacts: %s\n" "$release_dir" >&2' ERR
bundle="$release_dir/Oculi.app"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp "$binary" "$bundle/Contents/MacOS/oculi"
cp resources/Info.plist "$bundle/Contents/Info.plist"
/usr/bin/plutil -replace CFBundleShortVersionString -string "$version" "$bundle/Contents/Info.plist"
/usr/bin/plutil -replace CFBundleVersion -string "$build_number" "$bundle/Contents/Info.plist"
/usr/bin/plutil -lint "$bundle/Contents/Info.plist"

signing=(--sign - --timestamp=none)
if [[ "$mode" == publish ]]; then
    signing=(--sign "$OCULI_SIGNING_IDENTITY" --timestamp)
fi
/usr/bin/codesign --force "${signing[@]}" --options runtime \
    --entitlements resources/Release.entitlements "$bundle"
/usr/bin/codesign --verify --deep --strict --verbose=2 "$bundle"
/usr/bin/codesign --display --verbose=4 "$bundle" 2> "$release_dir/signature.txt"
/usr/bin/codesign --display --entitlements - --xml "$bundle" > "$release_dir/entitlements.plist"
/usr/bin/plutil -lint "$release_dir/entitlements.plist"

if [[ "$mode" == local ]]; then
    printf 'LOCAL ONLY: ad-hoc signed, NOT notarized or trusted for distribution.\n%s\n' "$bundle"
    exit 0
fi

/usr/bin/codesign --verify --strict \
    -R='anchor apple generic and certificate leaf[field.1.2.840.113635.100.6.1.13] exists' "$bundle"
submission="$release_dir/notary-submission.zip"
/usr/bin/ditto -c -k --sequesterRsrc --keepParent "$bundle" "$submission"
if ! xcrun notarytool submit "$submission" --keychain-profile "$OCULI_NOTARY_PROFILE" \
    --wait --timeout 30m --output-format json > "$release_dir/notary-result.json"; then
    fail "Notarization failed or timed out. Inspect $release_dir/notary-result.json; artifacts retained. Do not distribute."
fi
status="$(/usr/bin/plutil -extract status raw -o - "$release_dir/notary-result.json")"
[[ "$status" == Accepted ]] || fail "Notarization status: $status. Inspect $release_dir/notary-result.json; do not distribute."
xcrun stapler staple "$bundle"
xcrun stapler validate "$bundle"
/usr/bin/codesign --verify --deep --strict --verbose=2 "$bundle"
/usr/sbin/spctl --assess --type execute --verbose=4 "$bundle"

archive="$release_dir/Oculi-$version-$build_number-macos-arm64.zip"
/usr/bin/ditto -c -k --sequesterRsrc --keepParent "$bundle" "$archive"
(cd "$release_dir" && /usr/bin/shasum -a 256 "$(basename "$archive")" > "$(basename "$archive").sha256")
printf 'Signed, notarized, stapled, and Gatekeeper-verified release:\n%s\n' "$archive"