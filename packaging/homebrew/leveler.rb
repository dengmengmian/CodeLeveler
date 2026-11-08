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
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v1.0.24/leveler-v1.0.24-aarch64-apple-darwin.tar.gz"
      sha256 "18586100d098c2ef4c9014de15ae6845037f01636cb0df0e664b5d2de042bc8e"
    else
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v1.0.24/leveler-v1.0.24-x86_64-apple-darwin.tar.gz"
      sha256 "8e0f9308e6a0c8f1268f55ba8d5f4061e5250b5677cca5f9baf83b6eea60d9dd"
    end
  end

  on_linux do
    on_arm do
      # No linux-aarch64 artifact is published yet. Homebrew validates every
      # platform branch and rejects the tap when one has no URL, so this branch
      # names the artifact it would use; `fetch` then fails there with a plain
      # 404 instead of taking the whole tap down.
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v1.0.24/leveler-v1.0.24-aarch64-unknown-linux-gnu.tar.gz"
      sha256 :no_check
    end
    on_intel do
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v1.0.24/leveler-v1.0.24-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0f83cb4d48bef20f2f3ee2294f7443460f9a7478f57e24ccd759619c54cad1bd"
    end
  end

  def install
    bin.install "leveler"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/leveler --version")
  end
end
