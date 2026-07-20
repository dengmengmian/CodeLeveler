class Leveler < Formula
  desc "Local-first coding agent CLI: terminal UI, typed tools, resumable sessions"
  homepage "https://github.com/dengmengmian/CodeLeveler"
  version "0.1.0"
  license "Apache-2.0"

  livecheck do
    url :stable
    strategy :github_latest
  end

  # Release archives are `leveler-v<version>-<triple>.tar.gz`, each unpacking to
  # a single `leveler-v<version>-<triple>/` dir holding the `leveler` binary.
  # Homebrew strips that single top-level dir, so `bin.install "leveler"` works.
  #
  # SHA256s below are placeholders. After a release is published, regenerate this
  # file with `packaging/homebrew/update-formula.sh v<version>` (it pulls the
  # real digests from the release's *.sha256 assets), then copy it into the tap's
  # Formula/leveler.rb.
  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v#{version}/leveler-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    else
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v#{version}/leveler-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  on_linux do
    # Only x86_64 Linux has a prebuilt binary; arm64 Linux builds from source
    # via `cargo install` (see the README) or `brew install --build-from-source`.
    on_intel do
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v#{version}/leveler-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    bin.install "leveler"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/leveler --version")
  end
end
