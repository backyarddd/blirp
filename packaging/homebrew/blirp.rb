# Homebrew formula for the standalone blirp CLI/daemon (headless hubs, servers).
# Template: the VERSION and SHA256 placeholders are filled by scripts/render-packaging.sh
# in the release workflow; the rendered file is attached to each GitHub release.
class Blirp < Formula
  desc "Workspace and cross-session memory for CLI coding agents"
  homepage "https://github.com/backyarddd/blirp"
  version "{{VERSION}}"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/backyarddd/blirp/releases/download/v{{VERSION}}/blirp-{{VERSION}}-aarch64-apple-darwin.tar.gz"
      sha256 "{{SHA256_CLI_MACOS_ARM64}}"
    end
    on_intel do
      url "https://github.com/backyarddd/blirp/releases/download/v{{VERSION}}/blirp-{{VERSION}}-x86_64-apple-darwin.tar.gz"
      sha256 "{{SHA256_CLI_MACOS_X64}}"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/backyarddd/blirp/releases/download/v{{VERSION}}/blirp-{{VERSION}}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "{{SHA256_CLI_LINUX_ARM64}}"
    end
    on_intel do
      url "https://github.com/backyarddd/blirp/releases/download/v{{VERSION}}/blirp-{{VERSION}}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "{{SHA256_CLI_LINUX_X64}}"
    end
  end

  conflicts_with cask: "blirp", because: "the blirp app also links a `blirp` binary"

  def install
    bin.install "blirp"
  end

  def caveats
    <<~EOS
      Start the daemon now and at every login:
        blirp service install
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/blirp --version")
  end
end
