#!/bin/sh
# One content digest over the built WebUI, identical on Linux, macOS and Windows
# Git Bash. `release.yml` computes it once in `prepare-web` and every Rust build
# recomputes it from the downloaded artifact, so a stale, partial or tampered
# `dist` fails the release instead of being embedded by rust-embed.
#
# Usage: sh packaging/scripts/web_dist_hash.sh [dist-dir]
# Prints a single 64-hex digest.
set -eu

DIST="${1:-crates/leveler-web/web/dist}"

[ -f "$DIST/index.html" ] || {
  echo "web dist: no $DIST/index.html" >&2
  exit 1
}

# GNU `sha256sum` on Linux and Git Bash, `shasum` on macOS. Only the hex is
# kept: the two tools' filename columns differ, and the digest must not.
hash_tool() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum
  else
    shasum -a 256
  fi
}

find "$DIST" -type f | LC_ALL=C sort | while IFS= read -r file; do
  printf '%s  %s\n' "$(hash_tool < "$file" | awk '{print $1}')" "$file"
done | hash_tool | awk '{print $1}'
