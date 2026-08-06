#!/usr/bin/env bash
# Fetch real-world repositories used by scenario eval cases, pinned to fixed
# refs for reproducibility. Repos land in `fixtures/repos/<name>` (gitignored,
# never committed) and are cloned — not copied — so eval runs stay deterministic
# across machines and time.
#
# Usage:  scripts/fetch_eval_repos.sh            # fetch all
#         scripts/fetch_eval_repos.sh ripgrep    # fetch one
#
# The scenario YAML references `repo: fixtures/repos/<name>` + `base_ref: <ref>`.
# Keep the refs here in sync with the `base_ref` fields in evals/scenarios/**.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dest_root="$repo_root/fixtures/repos"
mkdir -p "$dest_root"

# name|git-url|pinned-ref   (ref must be a tag or full SHA present after clone)
REPOS=(
  "ripgrep|https://github.com/BurntSushi/ripgrep|14.1.1"
  # The real-project TUI matrix (fixtures/matrix/README.md) drives these.
  # Pinned so a case starts from the same tree on every machine.
  "rust-csv|https://github.com/BurntSushi/rust-csv|1.4.0"
  "rust-semver|https://github.com/dtolnay/semver|280ebcb"
  "rust-httparse|https://github.com/seanmonstar/httparse|a0fa552"
  "rust-similar|https://github.com/mitsuhiko/similar|3.1.1"
  "mux|https://github.com/gorilla/mux|db9d1d0"
  "jwt|https://github.com/golang-jwt/jwt|1a11d37"
  "decimal|https://github.com/shopspring/decimal|3090cc4"
  "go-humanize|https://github.com/dustin/go-humanize|4d1d908"
  "commander|https://github.com/tj/commander.js|v15.0.0"
  "ms|https://github.com/vercel/ms|4ff48ce"
  "negotiator|https://github.com/jshttp/negotiator|a690ff2"
)

fetch_one() {
  local name="$1" url="$2" ref="$3"
  local dir="$dest_root/$name"
  if [ -d "$dir/.git" ]; then
    echo "→ $name already present at $dir (skip; delete to re-fetch)"
  else
    echo "→ cloning $name @ $ref"
    git clone --quiet "$url" "$dir"
  fi
  git -C "$dir" fetch --quiet --tags origin || true
  git -C "$dir" checkout --quiet "$ref"
  echo "  $name at $(git -C "$dir" rev-parse --short HEAD) ($ref)"
}

want="${1:-}"
for entry in "${REPOS[@]}"; do
  IFS='|' read -r name url ref <<<"$entry"
  if [ -z "$want" ] || [ "$want" = "$name" ]; then
    fetch_one "$name" "$url" "$ref"
  fi
done
echo "done."
