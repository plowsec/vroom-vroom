#!/bin/sh
# Build only the optional kernel debugger, then sign it on macOS.
set -eu
cd "$(dirname "$0")/.."
if pgrep -f "^($(pwd)/)?target/(debug|release)/vroom-lab " >/dev/null 2>&1; then
  echo "a lab is running from target/: save a snapshot and stop it before rebuilding" >&2
  exit 1
fi
cargo build -p vroom-kernel "$@"
profile=debug
case " $* " in *" --release "*) profile=release ;; esac
target_dir=${CARGO_TARGET_DIR:-target}
run_dir="$target_dir/research/$profile"
mkdir -p "$run_dir"
# Cargo tests can relink target/debug/vroom-lab without its entitlement.
# Keep the signed runtime separate; atomic replacement also preserves a
# running process's executable inode.
temporary=$(mktemp "$run_dir/.vroom-lab.XXXXXX")
trap 'rm -f "$temporary"' EXIT HUP INT TERM
cp "$target_dir/$profile/vroom-lab" "$temporary"
chmod 755 "$temporary"
scripts/sign.sh "$temporary"
mv -f "$temporary" "$run_dir/vroom-lab"
echo "built: $run_dir/vroom-lab"
