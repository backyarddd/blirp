# Installing blirp

blirp is one program in two parts, installed together:

- **`blirp`**, the CLI: daemon, hook handler, MCP server and hub in one file. Enough on its own for servers, headless hubs and browser-only use (`blirp open`).
- **The desktop app**: a native window around the daemon's web UI, a tray icon and `blirp://` links. It starts the daemon when needed.

All data lives in `~/.blirp` (`%USERPROFILE%\.blirp` on Windows; override with `BLIRP_HOME`). Installing, updating and uninstalling never touch it, except `blirp uninstall --purge`.

## Install (recommended)

macOS and Linux (x86_64 or arm64):

```sh
curl -fsSL https://raw.githubusercontent.com/backyarddd/blirp/main/install.sh | sh
```

Windows 10 1809+ / 11, x64, in PowerShell (Windows PowerShell 5.1 or PowerShell 7):

```powershell
irm https://raw.githubusercontent.com/backyarddd/blirp/main/install.ps1 | iex
```

Then run `blirp`: it starts the daemon and opens the app.

The scripts download the latest published release, check every file against the release's `SHA256SUMS.txt` and that file against its minisign signature (when they can, see [below](#how-the-scripts-verify-downloads)), and install without admin rights:

| | CLI | Desktop app | PATH |
|---|---|---|---|
| macOS | `~/.local/bin/blirp` | `~/Applications/blirp.app` | printed instructions, or `--modify-path` |
| Linux | `~/.local/bin/blirp` | `~/.local/share/blirp/blirp.AppImage` + menu entry `~/.local/share/applications/blirp.desktop` | printed instructions, or `--modify-path` |
| Windows | `%LOCALAPPDATA%\Programs\blirp\blirp.exe` with `conpty.dll` and `x64\OpenConsole.exe` | `blirp-desktop.exe` in the same folder + Start Menu entry | added to your user `Path` |

They also write an install receipt (`install.json`: next to `blirp.exe` on Windows, in `~/.local/share/blirp/` on macOS/Linux) that `blirp update` and `blirp uninstall` read. Running a script again upgrades in place (a running daemon is stopped and started again with `blirp start`, through the autostart service when one is installed).

No code-signing prompts appear: files fetched with `curl` or `irm` carry no quarantine flag or Mark-of-the-Web, so Gatekeeper and SmartScreen do not ask. See [faq.md](faq.md#why-is-blirp-not-code-signed) for why blirp is not signed and how the downloads are verified instead.

### Options

| install.sh | install.ps1 | Environment variable | Effect |
|---|---|---|---|
| `--version X` | `-Version X` | `BLIRP_VERSION=X` | install release X instead of the latest (also downgrades) |
| `--no-app` | `-NoApp` | `BLIRP_NO_APP=1` | CLI only |
| `--service` | `-Service` | `BLIRP_SERVICE=1` | start the daemon at login (`blirp service install`) |
| `--modify-path` | (default) | `BLIRP_MODIFY_PATH=1` | add the CLI folder to `PATH` in your shell startup file (`~/.zshrc`, `~/.bashrc` or `~/.bash_profile`, fish `conf.d/blirp.fish`, else `~/.profile`); marked `# added by the blirp installer` |
| | `-NoModifyPath` | `BLIRP_NO_MODIFY_PATH=1` | Windows: leave the user `Path` alone |
| | | `BLIRP_INSTALL_DIR=dir` | install the CLI (Windows: everything) into `dir` |
| | | `BLIRP_REQUIRE_SIGNATURE=1` | refuse to install when the release signature cannot be checked |

Pass options to the piped scripts like this:

```sh
curl -fsSL https://raw.githubusercontent.com/backyarddd/blirp/main/install.sh | sh -s -- --service --modify-path
```
```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/backyarddd/blirp/main/install.ps1))) -Service
$env:BLIRP_NO_APP = '1'; irm https://raw.githubusercontent.com/backyarddd/blirp/main/install.ps1 | iex
```

`install.sh` needs `curl` (or `wget`), `tar` and `sha256sum` or `shasum`. The desktop app is installed even without a display (it is just a file); use `--no-app` on servers.

### How the scripts verify downloads

Every file is checked against the release's `SHA256SUMS.txt`. That file is signed with the blirp release key (`SHA256SUMS.txt.sig`, minisign), and the scripts check the signature with the key written into them whenever the machine can:

- with `minisign`, if it is installed;
- else with OpenSSL 1.1.1 or 3 (it needs BLAKE2b-512 and Ed25519): most Linux distributions, Homebrew's `openssl@3` on macOS (found even when it is not on `PATH`), and on Windows the OpenSSL that ships with Git for Windows. macOS's own `openssl` is LibreSSL and cannot; Windows PowerShell and .NET have no Ed25519.

A bad signature stops the install. When nothing can check it, the script says so and continues: the checksums then came over HTTPS from GitHub, just like the script itself, so they protect against a corrupted download but not against a compromised release. Set `BLIRP_REQUIRE_SIGNATURE=1` to refuse to install in that case. Once installed, `blirp update` always verifies the signature itself.

### Private repository, mirrors and testing

- `GITHUB_TOKEN`: sent as a bearer token to the GitHub API, and downloads go through the API asset URLs (`Accept: application/octet-stream`), so the scripts and `blirp update` work against a private repository (a fine-grained token with read access to its contents is enough). It also lifts the anonymous API rate limit. `install.sh` requires `curl` when a token is set.
- `BLIRP_RELEASE_BASE_URL`: replaces `https://api.github.com/repos/backyarddd/blirp/releases`. The scripts, `blirp update` and the daemon's update check request `<base>/latest` or `<base>/tags/v<version>` and expect GitHub's release JSON (`tag_name`, `html_url`, `assets[].name`, `.url`, `.browser_download_url`). Use it for a mirror or a local test server.

Only published releases are used: `latest` skips drafts and prereleases, and a draft cannot be installed even with `--version`.

## Manual download

Every release on [GitHub Releases](https://github.com/backyarddd/blirp/releases) has:

| Asset | What |
|---|---|
| `blirp-<version>-<target>.tar.gz` / `.zip` | CLI; targets `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-pc-windows-msvc` (the Windows zip also has `conpty.dll` and `x64\OpenConsole.exe`) |
| `blirp_<version>_aarch64.app.tar.gz`, `_x64.app.tar.gz` | macOS app bundle |
| `blirp_<version>_amd64.AppImage`, `_aarch64.AppImage` | Linux app |
| `blirp_<version>_x64-portable.zip` | Windows app: `blirp-desktop.exe`, `blirp.exe`, `conpty.dll`, `x64\OpenConsole.exe` |
| `blirp_<version>_x64-setup.exe`, `.msi`, `.dmg`, `.deb`, `.rpm` | classic installers (optional; unsigned, so the OS warns, see below) |
| `SHA256SUMS.txt`, `SHA256SUMS.txt.sig` | checksums of every asset, and their minisign signature |

Check a download:

```sh
sha256sum --ignore-missing -c SHA256SUMS.txt          # Linux
shasum -a 256 --ignore-missing -c SHA256SUMS.txt       # macOS
minisign -Vm SHA256SUMS.txt -p minisign.pub            # optional: the signature (key below)
```
```powershell
(Get-FileHash blirp-<version>-x86_64-pc-windows-msvc.zip -Algorithm SHA256).Hash   # compare with SHA256SUMS.txt
```

The release public key is [`packaging/minisign.pub`](../packaging/minisign.pub) (`RWR6n63Gj9buu2zM3R/x2t2GdHNFgEy1em/gM8aKhteWB+zielbybJVW`). `blirp update` checks this signature before it trusts any checksum.

Files downloaded with a browser are quarantined (macOS) or marked as from the internet (Windows), so the OS warns about unsigned programs: "blirp is damaged" or "cannot be opened" on macOS, "Windows protected your PC" on Windows. After checking the checksum, see [troubleshooting.md](troubleshooting.md#blirp-is-damaged-or-windows-protected-your-pc), or use the install scripts, which avoid this.

Other packaging: every release attaches a rendered Homebrew formula and cask and winget manifests; they are not published in a tap or `winget-pkgs` yet.

Classic installers and packages write no install receipt, so `blirp update` does not replace them: update by running the newer installer or through the package manager (`brew upgrade`, `apt`, `dnf`, `winget upgrade`).

## Build from source

Requirements: Rust 1.91+ (CI pins 1.94.0), Node.js 22, pnpm 12, and for the desktop app the [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/).

```sh
pnpm -C web install && pnpm -C web build     # the UI, embedded into the binary
cargo build --release -p blirp               # target/release/blirp
```

A source build has no install receipt: `blirp update` refuses to replace it (update with `git pull` and rebuild) and `blirp uninstall` leaves the binary in place. More in [development.md](development.md).

## After installing

```sh
blirp                   # start the daemon, open the app (or the browser UI)
blirp doctor            # data dir, config, database, git, detected agents
blirp status            # daemon pid, URL, version
```

Over SSH, or on Linux without a display, `blirp` starts the daemon and prints its URL and how to reach it from another device instead of opening anything.

Agents are detected from your `PATH`. The desktop app asks your login shell for its `PATH` when it starts the daemon, so CLIs installed through npm, Homebrew or `~/.local/bin` are found even when the app was started from the Dock or a launcher.

### Start at login

```sh
blirp service install     # macOS LaunchAgent, systemd --user unit, or HKCU Run entry
blirp service status
blirp service uninstall   # removes exactly that entry
```

`service install` records the absolute path of the `blirp` binary you ran it with, and on macOS/Linux the current `PATH` (so the daemon finds your agents) and `BLIRP_HOME` if set. Re-run it after moving the binary or changing where your agents are installed. It is idempotent.

On Windows the entry runs `conhost.exe --headless blirp.exe daemon --detach`, so no console window appears at login. If you use a custom `BLIRP_HOME` on Windows, set it as a user environment variable so the login entry sees it.

### Windows Firewall

A standalone daemon only listens on `127.0.0.1`, so Windows asks nothing. Windows Defender Firewall asks once, the first time `blirp.exe` listens on the network: when you enable the hub, pair with a hub, or turn on the LAN portal. Allow it on **Private networks**, and make sure your network is marked Private (Settings > Network & internet > your connection > Network profile type); Windows marks unknown networks Public, where this rule does not apply. The rule belongs to the path of `blirp.exe`; `blirp update` and the install script put the new version at the same path, so it keeps applying after updates. If you clicked Cancel, see [troubleshooting.md](troubleshooting.md#windows-firewall-blocks-sync-or-the-portal).

## Updating

```sh
blirp update --check      # exit 0: up to date, 10: an update is available
blirp update              # install the latest release
blirp update --version 0.3.1   # a specific release, also older ones
```

`blirp update` downloads `SHA256SUMS.txt`, verifies its signature with the release key built into blirp, downloads the CLI archive (and the desktop app, if the install script installed it), and checks both against the sums. Only then does it stop the daemon (running sessions end as Detached and can be resumed), replace the files and start the daemon again (through the autostart service when one is installed). If replacing any file fails, the files already replaced are put back and the old version starts again. On Windows the running `blirp.exe` is renamed to `blirp.exe.old` and removed the next time blirp starts. It never downgrades unless you pass `--version`, and it only updates installs made by the install scripts; otherwise it tells you how that copy was installed.

**Settings > About** shows when a newer release exists, with `blirp update` for script installs, else a pointer to the release page; the daemon asks GitHub for it at most once a day. Turn that off with `[update] check = false` ([configuration.md](configuration.md#update)). The desktop app has no updater of its own.

The database migrates forward automatically on the first start of a newer version.

## Uninstalling

```sh
blirp uninstall           # keeps your data in ~/.blirp
blirp uninstall --purge   # also deletes ~/.blirp (asks first; --yes skips the question)
```

It stops the daemon, removes the autostart entry, removes blirp's global agent hooks and MCP entries (only blirp's own entries; everything else in those files stays), then deletes what the install script installed: the CLI, the desktop app, its menu entry or Start Menu shortcut and `blirp://` registration, the `PATH` line or user `Path` entry the script added, and the receipt. It only deletes known file names in the recorded locations; a `blirp` that the scripts did not install (a source build, a package) is left in place. On Windows `blirp.exe` itself is deleted right after the command exits.

Installed with a classic installer instead: `blirp service uninstall` and `blirp hooks uninstall`, quit blirp from the tray, then remove it with the OS (Apps & features; move `blirp.app` to the Trash; `sudo apt remove blirp`).

## Troubleshooting

See [troubleshooting.md](troubleshooting.md): `blirp` not found after installing (PATH), antivirus warnings, Windows Firewall, "blirp is damaged", the daemon does not start, agents not detected.
