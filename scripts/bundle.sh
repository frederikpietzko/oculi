#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
export MACOSX_DEPLOYMENT_TARGET=15.0
profile="${1:-debug}"
case "$profile" in
    debug) cargo build --locked ;;
    release) cargo build --locked --release ;;
    *) printf 'Usage: bash scripts/bundle.sh [debug|release]\n' >&2; exit 2 ;;
esac
target_dir="$(cargo metadata --no-deps --format-version 1 | /usr/bin/plutil -extract target_directory raw -o - -)"
bundle="$target_dir/$profile/Oculi.app"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp "$target_dir/$profile/oculi" "$bundle/Contents/MacOS/oculi"
cp resources/Info.plist "$bundle/Contents/Info.plist"
/usr/bin/plutil -lint "$bundle/Contents/Info.plist"
/usr/bin/codesign --force --sign - "$bundle"
printf '%s\n' "$bundle"