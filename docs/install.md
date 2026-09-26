# Installing blirp

blirp ships in two forms from the same release:

- **Desktop app** (Tauri): a native window around the web UI, a tray icon, `blirp://` links and signed self-updates. It bundles the `blirp` binary and starts it as a background daemon.
- **Standalone binary** `blirp`: daemon, CLI, hook handler and MCP server in one file. Use it on servers and headless hubs, or if you prefer the browser UI (`blirp open`).

Both keep all data in `~/.blirp` (`%USERPROFILE%\.blirp` on Windows; override with `BLIRP_HOME`). Installing, upgrading or removing blirp never touches that directory.

Download everything from [GitHub Releases](https://github.com/backyarddd/blirp/releases). Each release has `SHA256SUMS.txt` and a `.sha256` file next to every CLI archive:

```sh
sha256sum -c blirp-0.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256      # Linux
shasum -a 256 -c blirp-0.1.0-aarch64-apple-darwin.tar.gz.sha256       # macOS
```
```powershell
(Get-FileHash blirp-0.1.0-x86_64-pc-windows-msvc.zip -Algorithm SHA256).Hash   # compare with the .sha256 file
```

## Windows (10 1809 or newer, x64)

**Desktop app.** Run `blirp_<version>_x64-setup.exe`. It installs for the current user into `%LOCALAPPDATA%\blirp` without admin rights and registers `blirp://` links. The `.msi` installs per machine instead. WebView2 is part of Windows 11; on Windows 10 the installer fetches it if missing.

The app folder also contains `blirp.exe` (the CLI) plus `conpty.dll` and `x64\OpenConsole.exe`, a current ConPTY build from Microsoft that fixes rendering and resize bugs of older inbox consoles. To use the CLI from a terminal, add `%LOCALAPPDATA%\blirp` to your user `PATH`.

**Standalone.** Unzip `blirp-<version>-x86_64-pc-windows-msvc.zip` somewhere permanent (for example `%LOCALAPPDATA%\Programs\blirp`) and add it to `PATH`. Keep `conpty.dll` and the `x64` folder next to `blirp.exe`; without them blirp falls back to the system ConPTY.

**winget.** Manifests are generated for every release (`winget-*.yaml` assets). Until the package is accepted into `microsoft/winget-pkgs`, use the installer directly.

Unsigned builds: if a release was built without a code-signing certificate, SmartScreen shows "Windows protected your PC"; choose **More info > Run anyway** after verifying the checksum.

## macOS (11 Big Sur or newer)

**Desktop app.** Open `blirp_<version>_aarch64.dmg` (Apple silicon) or `blirp_<version>_x64.dmg` (Intel) and drag blirp to Applications. The CLI is inside the bundle; link it onto your `PATH`:

```sh
sudo ln -sf /Applications/blirp.app/Contents/MacOS/blirp /usr/local/bin/blirp
```

**Standalone.**

```sh
tar -xzf blirp-<version>-aarch64-apple-darwin.tar.gz
install -m 0755 blirp-<version>-aarch64-apple-darwin/blirp /usr/local/bin/blirp
```

**Homebrew.** A formula (CLI) and a cask (app, which also links `blirp`) are generated for every release (`homebrew-*.rb` assets). Once they are published in a tap:

```sh
brew install blirp/tap/blirp          # CLI / daemon
brew install --cask blirp/tap/blirp   # desktop app
```

Unsigned builds: if a release was built without an Apple Developer ID, macOS refuses to open it. Right-click the app, choose **Open**, confirm; or run `xattr -dr com.apple.quarantine /Applications/blirp.app`. Archives downloaded with `curl` are not quarantined.

## Linux (x64 or arm64, glibc 2.35+: Ubuntu 22.04, Debian 12, Fedora 36 or newer)

**Desktop app.**

```sh
sudo apt install ./blirp_<version>_amd64.deb     # Debian/Ubuntu; installs /usr/bin/blirp too
sudo dnf install ./blirp-<version>-1.x86_64.rpm  # Fedora/openSUSE
chmod +x blirp_<version>_amd64.AppImage && ./blirp_<version>_amd64.AppImage
```

The `.deb` and `.rpm` put `blirp` (the CLI) on `PATH`. The AppImage is self-contained; its embedded CLI runs from a temporary mount, so for `blirp service install` or terminal use install the standalone binary as well. The tray icon needs an AppIndicator/StatusNotifier host (GNOME: the "AppIndicator and KStatusNotifierItem Support" extension); without it, closing the window still keeps the daemon running and launching blirp again brings the window back.

**Standalone.**

```sh
tar -xzf blirp-<version>-x86_64-unknown-linux-gnu.tar.gz
install -m 0755 blirp-<version>-x86_64-unknown-linux-gnu/blirp ~/.local/bin/blirp
```

## After installing

```sh
blirp daemon --detach   # start the daemon (the desktop app does this for you)
blirp doctor            # data dir, config, database, git, detected agents
blirp open              # the UI in your browser, already logged in
```

Agents are detected from your `PATH`. The desktop app asks your login shell for its `PATH` when it starts the daemon, so CLIs installed through npm, Homebrew or `~/.local/bin` are found even when the app is started from the Dock or a launcher.

### Start at login

```sh
blirp service install     # macOS LaunchAgent, systemd --user unit, or HKCU Run entry
blirp service status
blirp service uninstall   # removes exactly that entry; a running daemon keeps running
```

`service install` records the absolute path of the `blirp` binary you ran it with, and on macOS/Linux the current `PATH` (so the daemon finds your agents) and `BLIRP_HOME` if set. Re-run it after moving the binary or changing where your agents are installed. It is idempotent.

On Windows the entry runs `conhost.exe --headless blirp.exe daemon --detach`, so no console window appears at login. If you use a custom `BLIRP_HOME` on Windows, set it as a user environment variable so the login entry sees it.

## Upgrading

- Desktop app: accept the update prompt. Installing stops the daemon (ending running sessions) so its binary can be replaced; the app restarts it.
- Standalone: replace the binary, then restart the daemon (`blirp service` users: log out and in, or `launchctl kickstart -k gui/$(id -u)/dev.blirp.daemon`, or `systemctl --user restart blirp`).

The database migrates forward automatically on first start of a newer version.

## Uninstalling

1. `blirp service uninstall` if you enabled autostart.
2. Quit blirp from the tray (or stop the daemon).
3. Remove the app (Windows: Apps & features; macOS: move blirp.app to the Trash; Linux: `sudo apt remove blirp` / `sudo dnf remove blirp`) or delete the standalone binary.
4. Global agent hooks, if you enabled them: run `blirp hooks uninstall` before removing the binary.
5. Your memory and settings stay in `~/.blirp`. Delete that directory only if you want them gone.

## Troubleshooting

- **The app shows "blirp could not start its daemon".** The page shows the error and the log folder (`~/.blirp/logs`: `blirpd.*.log` for the daemon, `desktop.*.log` for the app). Common causes: an invalid `~/.blirp/config.toml` (the error names the key), or another daemon for the same `BLIRP_HOME` started by a different blirp version.
- **An agent is missing from the new-session dialog.** Run `blirp doctor` in the same environment the daemon runs in. For autostarted daemons, re-run `blirp service install` from a shell where the agent is on `PATH`.
- **Port 47770 is taken.** The daemon falls back to a free port and writes it to `~/.blirp/runtime.json`; `blirp status` shows it. Set `[daemon] port` in `config.toml` to pick one.
