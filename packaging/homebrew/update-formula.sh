#!/usr/bin/env bash
# Regenerate packaging/homebrew/leveler.rb from a PUBLISHED GitHub release.
#
# It downloads the release's *.sha256 assets and writes the real digests into
# the formula, so you never hand-copy a hash. Requires the `gh` CLI, logged in.
#
# Usage:  packaging/homebrew/update-formula.sh v0.1.0
# Then:   copy the regenerated leveler.rb into your tap's Formula/leveler.rb.
set -euo pipefail

TAG="${1:?usage: update-formula.sh vX.Y.Z}"
VER="${TAG#v}"
REPO="dengmengmian/CodeLeveler"
HERE="$(cd "$(dirname "$0")" && pwd)"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
gh release download "$TAG" -R "$REPO" --pattern '*.tar.gz.sha256' -D "$tmp"

sha() {
  # First field of `<sha>  <name>` in the matching .sha256 file.
  awk '{print $1}' "$tmp/leveler-v${VER}-$1.tar.gz.sha256"
}

SHA_ARM_MAC="$(sha aarch64-apple-darwin)"
SHA_X86_MAC="$(sha x86_64-apple-darwin)"
SHA_X86_LINUX="$(sha x86_64-unknown-linux-gnu)"

cat > "$HERE/leveler.rb" <<EOF
class Leveler < Formula
  desc "Local-first coding agent CLI: terminal UI, typed tools, resumable sessions"
  homepage "https://github.com/dengmengmian/CodeLeveler"
  version "${VER}"
  license "Apache-2.0"

  livecheck do
    url :stable
    strategy :github_latest
  end

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v#{version}/leveler-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "${SHA_ARM_MAC}"
    else
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v#{version}/leveler-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "${SHA_X86_MAC}"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v#{version}/leveler-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "${SHA_X86_LINUX}"
    end
  end

  def install
    bin.install "leveler"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/leveler --version")
  end
end
EOF

echo "Wrote $HERE/leveler.rb for ${TAG}"
echo "  aarch64-apple-darwin      ${SHA_ARM_MAC}"
echo "  x86_64-apple-darwin       ${SHA_X86_MAC}"
echo "  x86_64-unknown-linux-gnu  ${SHA_X86_LINUX}"
echo "Next: copy leveler.rb into the tap's Formula/leveler.rb and commit."
