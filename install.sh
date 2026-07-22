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

# Verification is mandatory: never install an executable when its checksum is
# missing, malformed, or cannot be calculated on this host.
checksum="$tmp/${name}.tar.gz.sha256"
curl -fsSL "${base}/${name}.tar.gz.sha256" -o "$checksum" 2>/dev/null \
  || die "checksum download failed; refusing unverified install"
want="$(cut -d' ' -f1 "$checksum")"
case "$want" in
  ""|*[!0-9a-fA-F]*) die "invalid sha256 checksum file" ;;
esac
[ "${#want}" -eq 64 ] || die "invalid sha256 checksum file"
want="$(printf '%s' "$want" | tr 'A-F' 'a-f')"

if have shasum; then
  got="$(shasum -a 256 "$tmp/${name}.tar.gz" | cut -d' ' -f1)" \
    || die "could not calculate sha256"
elif have sha256sum; then
  got="$(sha256sum "$tmp/${name}.tar.gz" | cut -d' ' -f1)" \
    || die "could not calculate sha256"
else
  die "shasum or sha256sum is required; refusing unverified install"
fi
case "$got" in
  ""|*[!0-9a-fA-F]*) die "hash tool returned an invalid sha256" ;;
esac
[ "${#got}" -eq 64 ] || die "hash tool returned an invalid sha256"
got="$(printf '%s' "$got" | tr 'A-F' 'a-f')"
[ "$want" = "$got" ] || die "sha256 mismatch (want $want, got $got)"

tar -xzf "$tmp/${name}.tar.gz" -C "$tmp"
mkdir -p "$BIN_DIR"
mv "$tmp/${name}/leveler" "$BIN_DIR/leveler"
chmod +x "$BIN_DIR/leveler"

# macOS: a quarantine flag (set when an archive is downloaded via a browser)
# trips Gatekeeper's "unverified developer" prompt on first run. A curl install
# usually carries none, but strip it either way so `leveler` runs immediately.
if [ "$os" = Darwin ]; then
  xattr -d com.apple.quarantine "$BIN_DIR/leveler" 2>/dev/null || true
fi

echo "Installed leveler ${ver} -> $BIN_DIR/leveler"
case ":$PATH:" in
  *":$BIN_DIR:"*) : ;;
  *) echo "Add $BIN_DIR to your PATH, e.g.:  export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac
echo "Next: run 'leveler doctor' to check your setup."
