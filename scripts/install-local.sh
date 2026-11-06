#!/usr/bin/env bash
# Install the harlettizer you use every day, out of cargo's target/.
#
# Whatever names the installed copy keeps working when a checkout is cleaned or
# rebuilt. A build under target/ (cargo build, cargo run) is a dev copy.
#
#   scripts/install-local.sh               build main and install it
#   scripts/install-local.sh --any-branch  install the current branch anyway
#
# Built in release with the `opus` feature, for `iamf --codec opus`: it links
# the system's libopus, which needs its development files to build (see
# docs/iamf.md#opus; OPUS_LIB_DIR and OPUS_STATIC reach the build as they are).
#
# Installs into $PREFIX (default ~/.local):
#   bin/harlettizer
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"

any_branch=0
case "${1:-}" in
  "") ;;
  --any-branch) any_branch=1 ;;
  -h|--help) awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "$0"; exit 0 ;;
  *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
esac

cd "$REPO"
branch="$(git symbolic-ref --short -q HEAD || echo '(detached HEAD)')"
if [ "$any_branch" = 0 ]; then
  if [ "$branch" != main ]; then
    echo "on $branch, not main: the installed copy is the one you rely on." >&2
    echo "Switch to main, or pass --any-branch to install this branch anyway." >&2
    exit 1
  fi
  if [ -n "$(git status --porcelain)" ]; then
    echo "the checkout has uncommitted changes; commit or stash them first," >&2
    echo "or pass --any-branch to install them anyway." >&2
    exit 1
  fi
fi
version="$(git describe --always --dirty)"

cargo build --release --locked -p hz-cli --features opus
# Wherever cargo's configuration puts target/.
target="$(cargo metadata --format-version 1 --no-deps |
  sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
built="${target:-$REPO/target}/release/harlettizer"
[ -x "$built" ] || { echo "the build left no $built" >&2; exit 1; }

bin="$PREFIX/bin/harlettizer"
install -d "$PREFIX/bin"
# Copied beside the old one and renamed over it: a run in progress keeps its
# file. Run once first, so a copy that cannot start (libopus missing at run
# time) never replaces one that can.
install -m 755 "$built" "$bin.new"
if ! "$bin.new" --version >/dev/null; then
  rm -f "$bin.new"
  echo "the new build does not start; $bin is left as it was" >&2
  exit 1
fi
mv -f "$bin.new" "$bin"

echo "installed harlettizer $version ($branch) as $bin"
found="$(command -v harlettizer || true)"
if [ "$found" != "$bin" ]; then
  echo "note: harlettizer on PATH is ${found:-nothing}, not $bin;" \
    "name the installed copy by its full path where it matters." >&2
fi
