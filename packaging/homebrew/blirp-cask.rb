# Homebrew cask for the blirp desktop app (goes to Casks/blirp.rb in a tap).
# Template: the VERSION and SHA256 placeholders are filled by scripts/render-packaging.sh
# in the release workflow; the rendered file is attached to each GitHub release.
cask "blirp" do
  arch arm: "aarch64", intel: "x64"

  version "{{VERSION}}"
  sha256 arm:   "{{SHA256_DMG_ARM64}}",
         intel: "{{SHA256_DMG_X64}}"

  url "https://github.com/blirp/blirp/releases/download/v#{version}/blirp_#{version}_#{arch}.dmg"
  name "blirp"
  desc "Workspace and cross-session memory for CLI coding agents"
  homepage "https://github.com/blirp/blirp"

  # The app updates itself (signed updater); brew should not fight it.
  auto_updates true
  depends_on macos: ">= :big_sur"
  conflicts_with formula: "blirp"

  app "blirp.app"
  # The bundled daemon/CLI, so `blirp service install`, `blirp open` etc. work.
  binary "#{appdir}/blirp.app/Contents/MacOS/blirp"

  # Agent memory in ~/.blirp is user data and is never removed by zap.
  zap trash: [
    "~/Library/Application Support/dev.blirp.desktop",
    "~/Library/Caches/dev.blirp.desktop",
    "~/Library/LaunchAgents/dev.blirp.daemon.plist",
    "~/Library/Saved Application State/dev.blirp.desktop.savedState",
    "~/Library/WebKit/dev.blirp.desktop",
  ]
end
