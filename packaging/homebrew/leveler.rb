class Leveler < Formula
  desc "Local-first coding agent CLI: terminal UI, typed tools, resumable sessions"
  homepage "https://github.com/dengmengmian/CodeLeveler"
  license "Apache-2.0"

  livecheck do
    url :stable
    strategy :github_latest
  end

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v1.0.20/leveler-v1.0.20-aarch64-apple-darwin.tar.gz"
      sha256 "6351c58372e77f170e11b41de712a0c16b35592f00d60eda7caec5ff371ea8be"
    else
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v1.0.20/leveler-v1.0.20-x86_64-apple-darwin.tar.gz"
      sha256 "2200627f56d3b15594915320bb04ac4e221cdcda71b01956ad261f08635a88ac"
    end
  end

  on_linux do
    on_arm do
      # No linux-aarch64 artifact is published yet. Homebrew validates every
      # platform branch and rejects the tap when one has no URL, so this branch
      # names the artifact it would use; `fetch` then fails there with a plain
      # 404 instead of taking the whole tap down.
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v1.0.20/leveler-v1.0.20-aarch64-unknown-linux-gnu.tar.gz"
      sha256 :no_check
    end
    on_intel do
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v1.0.20/leveler-v1.0.20-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "dbffa7b6cc2137bb6063c3429ae7bdc430b2562f0d87ee971b52c9e47c2430a9"
    end
  end

  def install
    bin.install "leveler"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/leveler --version")
  end
end
