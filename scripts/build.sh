#!/bin/sh
# Build everything and sign the VMM with the hypervisor entitlement (macOS).
# Usage: scripts/build.sh [--release]
set -e
cd "$(dirname "$0")/.."
cargo build "$@" -p bulwark-cli -p bulwark-gui
dir=target/debug
case " $* " in *" --release "*) dir=target/release ;; esac
scripts/sign.sh "$dir/bulwark"
echo "built: $dir/bulwark $dir/bulwark-gui"
