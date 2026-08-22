#!/bin/sh
# Every file `release.yml` puts in a release archive must exist in the tree.
#
# This exists because it did not. `NOTICE` was removed on 2026-08-05 by a sweep
# of unreferenced root files; `release.yml` still copied it into all four
# archives. Nothing noticed for seventeen days, because the release workflow
# only runs on a tag — so the break surfaced as four failed builds at the moment
# a release was actually wanted, which is the worst possible time to find it.
#
# The file list is read out of `release.yml` rather than restated here: a copy
# would drift, and a drifted guard is worse than none.
set -eu

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
WORKFLOW="$ROOT/.github/workflows/release.yml"

[ -f "$WORKFLOW" ] || { echo "missing $WORKFLOW" >&2; exit 1; }

# The unix line (`cp README.md LICENSE-APACHE NOTICE "dist/..."`) and the
# windows one (`Copy-Item README.md,LICENSE-APACHE,NOTICE "dist/..."`) name the
# same payload in two syntaxes. Check both, so they cannot silently diverge.
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
