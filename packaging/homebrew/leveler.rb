class Leveler < Formula
  desc "Local-first coding agent CLI: terminal UI, typed tools, resumable sessions"
  homepage "https://github.com/dengmengmian/CodeLeveler"
  version "1.0.16"
  license "Apache-2.0"

  livecheck do
    url :stable
    strategy :github_latest
  end

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v#{version}/leveler-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "650930363fd61925a16e0a58ebd5e1d3761b7b0d7486aa2600de02c96f0a9024"
    else
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v#{version}/leveler-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "4b30919b0cfdca24d2b87158ee090ec8b8f486f17d97cfcd83f46de5565ba645"
    end
  end

  on_linux do
    on_arm do
      # No linux-aarch64 artifact is published yet. Homebrew validates every
      # platform branch and rejects the tap when one has no URL, so this branch
      # names the artifact it would use; `fetch` then fails there with a plain
      # 404 instead of taking the whole tap down.
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v#{version}/leveler-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 :no_check
    end
    on_intel do
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v#{version}/leveler-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "7ec84a7bc847752d14e6b8aa2c0f8168c86fc7c8161dcfe8a519a4b2c317b063"
    end
  end

  def install
    bin.install "leveler"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/leveler --version")
  end
end
