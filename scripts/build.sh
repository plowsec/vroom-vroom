#!/bin/sh
# Build everything and sign the VMM with the hypervisor entitlement (macOS).
# Usage: scripts/build.sh [--release]
set -e
cd "$(dirname "$0")/.."
# macOS kills a process whose binary is rewritten: a VM started from
# target/ would die mid-run (or mid-suspend).
# Only this checkout's binaries (other worktrees have their own target/).
if pgrep -f "^($(pwd)/)?target/(debug|release)/vroom run" >/dev/null 2>&1; then
  echo "a VM is running from target/: stop or suspend it first (or set CARGO_TARGET_DIR)" >&2
  exit 1
fi
cargo build "$@" -p vroom-cli -p vroom-gui -p vroom-netd
dir=target/debug
case " $* " in *" --release "*) dir=target/release ;; esac
scripts/sign.sh "$dir/vroom"
echo "built: $dir/vroom $dir/vroom-gui"
