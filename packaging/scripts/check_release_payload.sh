#!/bin/sh
# Every file `release.yml` puts in a release archive must exist in the tree.
# The file list is read out of `release.yml` rather than restated here.
set -eu

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)"
WORKFLOW="$ROOT/.github/workflows/release.yml"

[ -f "$WORKFLOW" ] || { echo "missing $WORKFLOW" >&2; exit 1; }

# The unix `cp README.md …` line and the windows `Copy-Item README.md,…` line
# name the same payload in two syntaxes. Check both, so they cannot silently
# diverge.
#
# Anchored on `README.md` so the neighbouring lines that copy the *binary* out
# of `target/` are not mistaken for tracked files: those are build output, and
# their paths carry unexpanded `${{ matrix.target }}`.
unix_files="$(sed -n 's/^ *cp \(.*README\.md.*\) "dist\/.*$/\1/p' "$WORKFLOW" | tr -s ' ' '\n' | grep -v '^$' || true)"
win_files="$(sed -n 's/^ *Copy-Item \(.*README\.md.*\) "dist\/.*$/\1/p' "$WORKFLOW" | tr ',' '\n' | tr -s ' ' '\n' | grep -v '^$' || true)"

[ -n "$unix_files" ] || { echo "could not find the unix packaging line in release.yml" >&2; exit 1; }
[ -n "$win_files" ] || { echo "could not find the windows packaging line in release.yml" >&2; exit 1; }

status=0

check_list() {
  label="$1"
  list="$2"
  for f in $list; do
    if [ ! -e "$ROOT/$f" ]; then
      echo "release payload ($label): '$f' is packaged by release.yml but does not exist" >&2
      status=1
    fi
  done
}

check_list unix "$unix_files"
check_list windows "$win_files"

# The two platforms must ship the same payload.
unix_sorted="$(printf '%s\n' $unix_files | sort)"
win_sorted="$(printf '%s\n' $win_files | sort)"
if [ "$unix_sorted" != "$win_sorted" ]; then
  echo "release payload: the unix and windows archives ship different files" >&2
  echo "  unix:    $(echo $unix_sorted)" >&2
  echo "  windows: $(echo $win_sorted)" >&2
  status=1
fi

[ "$status" -eq 0 ] && echo "release payload: every packaged file exists, and both platforms agree"
exit "$status"
