#!/bin/sh
# Ad-hoc sign a binary with the hypervisor entitlement (macOS only).
set -e
[ "$(uname)" = Darwin ] || exit 0
codesign -f -s - --entitlements "$(dirname "$0")/entitlements.plist" "$@" 2>/dev/null
