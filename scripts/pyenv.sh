# Sourced by the build scripts: the Python that Mesa's and virglrenderer's
# code generators run on, in a venv under target/.
#
#   pyvenv target/pyenv
#   export PATH="target/pyenv/bin:$PATH"
#
# uv makes it, with uv's own CPython -- pinned here, fetched once into
# ~/.local/share/uv -- rather than whatever python3 is on PATH. That is the
# point of using uv at all: a Homebrew python can carry a pyexpat built
# against a newer libexpat than the /usr/lib one it loads at run time (a
# macOS point update is enough to put them out of step), and then plistlib,
# platform.mac_ver() and pip's vendored truststore fall over in turn, so
# `python3 -m venv` dies inside ensurepip and leaves a venv with no pip in
# it. An interpreter we pin and uv fetches is the same on every machine and
# owes Homebrew nothing. Remaking it costs about 50ms from uv's cache, so
# it is remade every time rather than checked.
#
# Pinned like the rest of this repo's dependencies; bump them together with
# the Mesa and virglrenderer revisions that need them.
PYVENV_PYTHON=3.13
# mako: Mesa's templates. yaml: the virgl format tables and Mesa's registry.
# packaging: Mesa's version comparisons.
PYVENV_PKGS="mako==1.4.3 pyyaml==6.0.3 packaging==26.3"

# pyvenv <dir>
pyvenv() {
  command -v uv >/dev/null 2>&1 || { echo "needs uv: brew install uv" >&2; return 1; }
  uv venv -q --clear --managed-python --python "$PYVENV_PYTHON" "$1"
  uv pip install -q --python "$1/bin/python3" $PYVENV_PKGS
}
