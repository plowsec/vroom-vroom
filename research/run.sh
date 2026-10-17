#!/bin/sh
set -eu
repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-"$repo_dir/target"}
case "$target_dir" in /*) ;; *) target_dir="$PWD/$target_dir" ;; esac
export CARGO_TARGET_DIR="$target_dir"
"$repo_dir/research/build.sh"
exec uv run --project "$repo_dir/research/tui" vroom-kernel \
  --backend "$target_dir/research/debug/vroom-lab" "$@"
