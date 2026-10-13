class Leveler < Formula
  desc "Local-first coding agent CLI: terminal UI, typed tools, resumable sessions"
  homepage "https://github.com/dengmengmian/CodeLeveler"
  version "1.0.15"
  license "Apache-2.0"

  livecheck do
    url :stable
    strategy :github_latest
  end

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v#{version}/leveler-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "fa4b11d5322281bc979a3484f07fa8ae97a14991623d4231ee7cd9c0f386a42d"
    else
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v#{version}/leveler-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "1714fad3be82fb80f7c6a4a3285b9ae36759c79fa9520cf68744525a01d9270a"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/dengmengmian/CodeLeveler/releases/download/v#{version}/leveler-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "dc3abd11c7cb844ccd4093c46d362cde8eca8ecc227cc96620729f0c6f48a14d"
    end
  end

  def install
    bin.install "leveler"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/leveler --version")
  end
end
