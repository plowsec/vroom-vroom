#!/bin/sh
# Build everything and sign the VMM with the hypervisor entitlement (macOS).
# Usage: scripts/build.sh [--release]
set -e
cd "$(dirname "$0")/.."
cargo build "$@" -p vroom-cli -p vroom-gui
dir=target/debug
case " $* " in *" --release "*) dir=target/release ;; esac
scripts/sign.sh "$dir/vroom"
echo "built: $dir/vroom $dir/vroom-gui"
