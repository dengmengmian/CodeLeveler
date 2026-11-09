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
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v1.0.25/leveler-v1.0.25-aarch64-apple-darwin.tar.gz"
      sha256 "5c86765391acb58a13840ee0658425ebc17e4ff7c46026152ffaf78fe08990ca"
    else
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v1.0.25/leveler-v1.0.25-x86_64-apple-darwin.tar.gz"
      sha256 "ee4eba0f65c7d0c36e751a0569fb581f662556b0ec8c40a40d9c5ab24c655ef3"
    end
  end

  on_linux do
    on_arm do
      # No linux-aarch64 artifact is published yet. Homebrew validates every
      # platform branch and rejects the tap when one has no URL, so this branch
      # names the artifact it would use; `fetch` then fails there with a plain
      # 404 instead of taking the whole tap down.
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v1.0.25/leveler-v1.0.25-aarch64-unknown-linux-gnu.tar.gz"
      sha256 :no_check
    end
    on_intel do
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v1.0.25/leveler-v1.0.25-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "3c2dabef311e2b03eb40b0ead2523a1568f41b1b5aa9f85f9a04828010970062"
    end
  end

  def install
    bin.install "leveler"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/leveler --version")
  end
end
