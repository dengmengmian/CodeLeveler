#!/bin/sh
# Run cargo against one target directory shared by every git worktree of this
# repository.
#
# Each linked worktree otherwise defaults to its own `target/`, so a fresh
# worktree recompiles the entire ~500-crate dependency graph from scratch.
# Cargo fingerprints registry dependencies by name and version, not by checkout
# path, so one shared directory lets every worktree reuse those artifacts; only
# the ~32 local crates are recompiled per worktree. Cargo still owns
# fingerprints, locking, and eviction — this script only selects the directory.
#
# Usage:
#   scripts/shared-target.sh check --workspace
#   scripts/shared-target.sh test --no-run -p leveler-tui
#
# Set LEVELER_TARGET_DIR to override the location. To make plain `cargo` (and
# editors such as rust-analyzer) use it too, export it once:
#   export CARGO_TARGET_DIR="$(scripts/shared-target.sh --print-dir)"
set -eu

print_dir_only=0
if [ "${1:-}" = "--print-dir" ]; then
    print_dir_only=1
    shift
fi

# `--git-common-dir` is the main worktree's `.git` for the main checkout and for
# every linked worktree, so this resolves to one directory for all of them.
common_dir=$(cd "$(git rev-parse --git-common-dir)" && pwd)
shared_dir="${LEVELER_TARGET_DIR:-$(dirname "$common_dir")/.codeleveler-target}"

if [ "$print_dir_only" = "1" ]; then
    printf '%s\n' "$shared_dir"
    exit 0
fi

export CARGO_TARGET_DIR="$shared_dir"
exec cargo "$@"
