# Sourced by the build scripts: skip a long build whose inputs (its script,
# patches, sources) haven't changed since it last ran.
#   if ! fresh target/x/.inputs scripts/build-x.sh third_party/x; then
#     scripts/build-x.sh && stamp target/x/.inputs scripts/build-x.sh third_party/x
#   fi

inputs_hash() {
  find "$@" -type f ! -name '.DS_Store' -print0 | sort -z | xargs -0 shasum -a 256 | shasum -a 256 | cut -d' ' -f1
}

# The stamp holds these inputs' hash.
fresh() {
  s=$1
  shift
  [ -f "$s" ] && [ "$(cat "$s")" = "$(inputs_hash "$@")" ]
}

stamp() {
  s=$1
  shift
  mkdir -p "$(dirname "$s")"
  inputs_hash "$@" >"$s"
}
