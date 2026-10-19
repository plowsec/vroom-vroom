#!/bin/sh
set -eu
if [ "${1:-}" = --help ]; then
  echo "usage: research/demo.sh CONFIG SNAPSHOT [TUI_OPTIONS...]"
  exit 0
fi
if [ "$#" -lt 2 ]; then
  echo "usage: research/demo.sh CONFIG SNAPSHOT [TUI_OPTIONS...]" >&2
  exit 2
fi
config=$1
snapshot=$2
shift 2
exec "$(dirname "$0")/run.sh" "$config" \
  --restore "$snapshot" --ex-callback-demo "$@"
