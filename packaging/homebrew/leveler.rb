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
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v1.0.23/leveler-v1.0.23-aarch64-apple-darwin.tar.gz"
      sha256 "e431e3078de2f4fb7c2475583b8a298911074281160e27dc9952da6bd74000b4"
    else
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v1.0.23/leveler-v1.0.23-x86_64-apple-darwin.tar.gz"
      sha256 "9ba5512ca26304b87455255b91a542f4d3d0fa6a0b7d2056d2d1464dd07adfda"
    end
  end

  on_linux do
    on_arm do
      # No linux-aarch64 artifact is published yet. Homebrew validates every
      # platform branch and rejects the tap when one has no URL, so this branch
      # names the artifact it would use; `fetch` then fails there with a plain
      # 404 instead of taking the whole tap down.
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v1.0.23/leveler-v1.0.23-aarch64-unknown-linux-gnu.tar.gz"
      sha256 :no_check
    end
    on_intel do
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v1.0.23/leveler-v1.0.23-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "54135e5fb19a1bcc3c6bfc66a6591e9bf5172d5e09b6ce7e60f410358b2db105"
    end
  end

  def install
    bin.install "leveler"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/leveler --version")
  end
end
