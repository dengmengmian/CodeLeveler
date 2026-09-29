#!/bin/sh
# Every file `release.yml` puts in a release archive must exist in the tree.
# The file list is read out of `release.yml` rather than restated here.
set -eu

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)"
WORKFLOW="$ROOT/.github/workflows/release.yml"

[ -f "$WORKFLOW" ] || { echo "missing $WORKFLOW" >&2; exit 1; }

# `rust-embed` captures the WebUI at Rust compile time. The SPA is built once
# in the `prepare-web` job, and every Rust build downloads that artifact before
# its first `cargo build --release` invocation. Both halves of that chain are
# checked here: a release that compiles leveler without a prepared, verified
# `web/dist` would embed a stale or empty SPA into every binary.
job_line() { # job name -> 1-based line of its YAML key, or empty
  grep -n "^  $1:" "$WORKFLOW" | head -1 | cut -d: -f1 || true
}
prepare_job_line="$(job_line prepare-web)"
build_job_line="$(job_line build)"
web_ci_line="$(grep -n -m1 'npm ci' "$WORKFLOW" | cut -d: -f1 || true)"
web_build_line="$(grep -n -m1 'npm run build' "$WORKFLOW" | cut -d: -f1 || true)"
rust_build_line="$(grep -n -m1 'cargo build --release --locked -p leveler-cli' "$WORKFLOW" | cut -d: -f1 || true)"

if [ -z "$prepare_job_line" ] || [ -z "$build_job_line" ] || [ -z "$web_ci_line" ] || [ -z "$web_build_line" ] || [ -z "$rust_build_line" ]; then
  echo "release payload: release.yml must have prepare-web and build jobs that build the WebUI before leveler" >&2
  exit 1
fi
if [ "$prepare_job_line" -ge "$build_job_line" ]; then
  echo "release payload: prepare-web must run before the build job" >&2
  exit 1
fi
if [ "$web_ci_line" -ge "$web_build_line" ] || [ "$web_ci_line" -le "$prepare_job_line" ] || [ "$web_build_line" -ge "$build_job_line" ]; then
  echo "release payload: prepare-web must npm ci and npm run build inside its own job" >&2
  exit 1
fi

# The build job must depend on prepare-web and fetch the artifact before it
# compiles: `needs` alone would still allow a reordered download step.
if ! sed -n "${build_job_line},${rust_build_line}p" "$WORKFLOW" | grep -q 'needs: \[version, prepare-web\]'; then
  echo "release payload: the build job must need [version, prepare-web]" >&2
  exit 1
fi
if ! sed -n "${build_job_line},${rust_build_line}p" "$WORKFLOW" | grep -q 'name: web-dist'; then
  echo "release payload: the build job must download the web-dist artifact before compiling" >&2
  exit 1
fi
if ! sed -n "${build_job_line},${rust_build_line}p" "$WORKFLOW" | grep -q 'web_dist_hash.sh'; then
  echo "release payload: the build job must verify the downloaded web-dist hash before compiling" >&2
  exit 1
fi

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
