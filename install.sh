#!/bin/sh
# CodeLeveler one-line installer.
#
#   curl -fsSL https://raw.githubusercontent.com/dengmengmian/CodeLeveler/main/install.sh | sh
#
# Detects your platform, downloads the matching prebuilt `leveler` binary from
# the latest GitHub release, verifies its sha256, and installs it to
# ~/.local/bin (override with LEVELER_BIN_DIR). No Rust toolchain required.
#
# Windows: download the .zip from the releases page instead (see the README).
set -eu

REPO="dengmengmian/CodeLeveler"
BIN_DIR="${LEVELER_BIN_DIR:-$HOME/.local/bin}"

die() { echo "install: $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

have curl || die "curl is required"
have tar || die "tar is required"

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Darwin)
    case "$arch" in
      arm64|aarch64) triple="aarch64-apple-darwin" ;;
      x86_64)        triple="x86_64-apple-darwin" ;;
      *) die "unsupported macOS arch: $arch (build from source: see README)" ;;
    esac ;;
  Linux)
    case "$arch" in
      x86_64) triple="x86_64-unknown-linux-gnu" ;;
      *) die "no prebuilt binary for linux/$arch; install with cargo (see README)" ;;
    esac ;;
  *) die "unsupported OS: $os; on Windows use the .zip from the releases page" ;;
esac

# Resolve the latest published release tag (drafts are not 'latest').
tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
  | grep '"tag_name"' | head -1 | cut -d'"' -f4)"
[ -n "$tag" ] || die "could not find a published release (is one published yet?)"
ver="${tag#v}"

name="leveler-${tag}-${triple}"
base="https://github.com/$REPO/releases/download/${tag}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading ${name}.tar.gz ..."
curl -fSL "${base}/${name}.tar.gz" -o "$tmp/${name}.tar.gz" || die "download failed"

# Verify sha256 when a checksum file and a hashing tool are both available.
if curl -fsSL "${base}/${name}.tar.gz.sha256" -o "$tmp/${name}.tar.gz.sha256" 2>/dev/null; then
  want="$(cut -d' ' -f1 "$tmp/${name}.tar.gz.sha256")"
  if have shasum; then got="$(shasum -a 256 "$tmp/${name}.tar.gz" | cut -d' ' -f1)"
  elif have sha256sum; then got="$(sha256sum "$tmp/${name}.tar.gz" | cut -d' ' -f1)"
  else got=""; fi
  if [ -n "$got" ] && [ "$want" != "$got" ]; then
    die "sha256 mismatch (want $want, got $got)"
  fi
fi

tar -xzf "$tmp/${name}.tar.gz" -C "$tmp"
mkdir -p "$BIN_DIR"
mv "$tmp/${name}/leveler" "$BIN_DIR/leveler"
chmod +x "$BIN_DIR/leveler"

echo "Installed leveler ${ver} -> $BIN_DIR/leveler"
case ":$PATH:" in
  *":$BIN_DIR:"*) : ;;
  *) echo "Add $BIN_DIR to your PATH, e.g.:  export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac
echo "Next: run 'leveler doctor' to check your setup."
