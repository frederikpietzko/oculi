#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
unset OCULI_SIGNING_IDENTITY OCULI_NOTARY_PROFILE OCULI_BUILD_NUMBER

expect_failure() {
    local expected_status="$1" expected_message="$2" output status=0
    shift 2
    output="$("$@" 2>&1)" || status=$?
    if [[ "$status" != "$expected_status" || "$output" != *"$expected_message"* ]]; then
        printf 'FAIL: expected exit %s and %s; got exit %s\n%s\n' \
            "$expected_status" "$expected_message" "$status" "$output" >&2
        exit 1
    fi
    printf 'PASS: %s\n' "$expected_message"
}

bash -n scripts/release.sh
bash scripts/release.sh --help
expect_failure 2 'Usage:' bash scripts/release.sh invalid
expect_failure 2 'Usage:' bash scripts/release.sh local extra
expect_failure 1 'Set OCULI_SIGNING_IDENTITY' bash scripts/release.sh
expect_failure 1 'Use a Developer ID Application identity' \
    env OCULI_SIGNING_IDENTITY=- bash scripts/release.sh publish
expect_failure 1 'Set OCULI_NOTARY_PROFILE' \
    env OCULI_SIGNING_IDENTITY='Developer ID Application: Test' bash scripts/release.sh publish
expect_failure 1 'Set OCULI_BUILD_NUMBER' \
    env OCULI_SIGNING_IDENTITY='Developer ID Application: Test' OCULI_NOTARY_PROFILE=test \
    bash scripts/release.sh publish
expect_failure 1 'not available in Keychain' \
    env OCULI_SIGNING_IDENTITY="Developer ID Application: Missing-$(uuidgen)" \
    OCULI_NOTARY_PROFILE=test OCULI_BUILD_NUMBER=1 bash scripts/release.sh publish
for invalid_build in 0 -1 1.2 '../escape'; do
    expect_failure 1 'OCULI_BUILD_NUMBER must be a positive integer' \
        env OCULI_BUILD_NUMBER="$invalid_build" bash scripts/release.sh local
done
printf 'PASS: release preflight checks (no build, signing, or network submission)\n'