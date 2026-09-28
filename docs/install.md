# Installing blirp

blirp is one program in two parts, installed together:

- **`blirp`**, the CLI: daemon, hook handler, MCP server and hub in one file. Enough on its own for servers, headless hubs and browser-only use (`blirp open`).
- **The desktop app**: a native window around the daemon's web UI, a tray icon and `blirp://` links. It starts the daemon when needed.

All data lives in `~/.blirp` (`%USERPROFILE%\.blirp` on Windows; override with `BLIRP_HOME`). Installing, updating and uninstalling never touch it, except `blirp uninstall --purge`.

## System requirements

| | CLI | Desktop app |
|---|---|---|
| Linux | x86_64 or arm64 with glibc 2.35 or newer (the release is built on Ubuntu 22.04): Ubuntu 22.04+, Debian 12+, Fedora 36+, RHEL/Rocky/Alma 10+. Not RHEL 9 (glibc 2.34) or musl distributions such as Alpine; build from source there. | **Experimental.** The same, plus a desktop session; tested in CI only so far. The CLI plus `blirp open` in a browser gives the same UI. The AppImage needs FUSE 2 to mount itself; `install.sh` installs it when it is missing ([prerequisites](#prerequisites), [troubleshooting](troubleshooting.md#linux-desktop)); the `.deb`/`.rpm` pull in WebKitGTK 4.1 and GTK 3 as dependencies. |
| macOS | 11 Big Sur or newer, Apple silicon or Intel | the same |
| Windows | 10 version 1809 or newer, or 11; x64 (on ARM64 the x64 build runs under emulation) | the same, plus the Microsoft Edge WebView2 Runtime. Windows 11 and up-to-date Windows 10 have it. `install.ps1` and the NSIS and MSI installers install it when it is missing; the portable zip does not ([troubleshooting](troubleshooting.md#windows-the-desktop-app-does-not-open-webview2)). |

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

The scripts download the latest published release, check every file against the release's `SHA256SUMS.txt` and that file against its minisign signature (see [below](#how-the-scripts-verify-downloads)), install missing [prerequisites](#prerequisites), and install blirp itself without admin rights:

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
| `--hub` | | `BLIRP_HUB=1` | server install (e.g. a VPS): CLI only, then `blirp hub setup` (autostart that survives logout, hub role, an invite); refuses to run as root ([vps.md](vps.md)) |
| `--modify-path` | (default) | `BLIRP_MODIFY_PATH=1` | add the CLI folder to `PATH` in your shell startup file (`~/.zshrc`, `~/.bashrc` or `~/.bash_profile`, fish `conf.d/blirp.fish`, else `~/.profile`); marked `# added by the blirp installer` |
| | `-NoModifyPath` | `BLIRP_NO_MODIFY_PATH=1` | Windows: leave the user `Path` alone |
| | | `BLIRP_INSTALL_DIR=dir` | install the CLI (Windows: everything) into `dir` |
| | | `BLIRP_REQUIRE_SIGNATURE=1` | refuse to install when the release signature cannot be checked |
| `--no-prereqs` | `-NoPrereqs` | `BLIRP_NO_PREREQS=1` | install no missing [prerequisites](#prerequisites) and fetch no minisign; only check, and stop when a required tool is missing |

Pass options to the piped scripts like this:

```sh
curl -fsSL https://raw.githubusercontent.com/backyarddd/blirp/main/install.sh | sh -s -- --service --modify-path
```
```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/backyarddd/blirp/main/install.ps1))) -Service
$env:BLIRP_NO_APP = '1'; irm https://raw.githubusercontent.com/backyarddd/blirp/main/install.ps1 | iex
```

The desktop app is installed even without a display (it is just a file); use `--no-app` on servers, or `--hub` to set up a server as your hub ([vps.md](vps.md)).

### Prerequisites

The scripts check what they and the desktop app need and install what is missing, printing the exact command first. When everything is there they change nothing and print nothing about it.

| | Needed | Installed when missing |
|---|---|---|
| Linux | `tar`, `gzip`, `curl` or `wget`, `sha256sum` and core tools (`awk`, `sed`, `mktemp`) | `tar`, `gzip`, `curl` with `apt-get`, `dnf`, `pacman` or `zypper`, as root or through `sudo`. When that fails the install stops and prints the command to run |
| Linux desktop app | FUSE 2 (`libfuse.so.2`) for the AppImage, which carries its other libraries (WebKitGTK, GTK) inside | `libfuse2` (`libfuse2t64` on Ubuntu 24.04+ and Debian 13+), `fuse fuse-libs` (dnf), `fuse2` (pacman), `libfuse2` (zypper). Not with `--no-app` or `--hub`, and not without a display (`DISPLAY` / `WAYLAND_DISPLAY` unset, e.g. over SSH; the script says so). When that fails the CLI and app are installed anyway, with a warning and the command |
| macOS | `curl`, `tar`, `gzip`, `shasum`, `unzip` | nothing: macOS ships them |
| Windows | Windows PowerShell 5.1 or PowerShell 7 | nothing |
| Windows desktop app | Microsoft Edge WebView2 Runtime | Microsoft's Evergreen bootstrapper, run only when it carries a valid Authenticode signature by Microsoft (without admin rights it installs for your user), else `winget install --id Microsoft.EdgeWebView2Runtime` (machine-wide, may ask for admin rights). When both fail the CLI and app are installed anyway, with a warning; `blirp open` works meanwhile |
| All | something to check the release signature | a pinned minisign fetched just for the check, see [below](#how-the-scripts-verify-downloads) |

`sudo` asks for your password on the terminal when it needs one (also with `curl ... | sh`); without a terminal it runs with `-n` and never waits. Package managers run with their non-interactive flags and no input (apt after `apt-get update`); on Arch, run `pacman -Syu` first if a package is not found. `--no-prereqs` / `-NoPrereqs` / `BLIRP_NO_PREREQS=1` turns all of this off: the scripts then only check, as before 0.2.1. Alpine and other musl distributions cannot run the release binary ([system requirements](#system-requirements)), so `apk` is not used.

Not prerequisites: `git` is optional (without it projects have no git features; `blirp doctor` says whether it is found), and agent CLIs such as `claude` or `codex` are yours to install; blirp lists the ones it finds on `PATH`.

### How the scripts verify downloads

Every file is checked against the release's `SHA256SUMS.txt`. That file is signed with the blirp release key (`SHA256SUMS.txt.sig`, minisign), and the scripts check the signature with the key written into them whenever the machine can:

- with `minisign`, if it is installed;
- else with OpenSSL 1.1.1 or 3 (it needs BLAKE2b-512 and Ed25519): most Linux distributions, Homebrew's `openssl@3` on macOS (found even when it is not on `PATH`), and on Windows the OpenSSL that ships with Git for Windows. macOS's own `openssl` is LibreSSL and cannot; Windows PowerShell and .NET have no Ed25519;
- else with minisign 0.12 from its author's GitHub release, fetched into the script's temp folder, checked against a SHA-256 written into the script (the build the release workflow signs with) and deleted with the temp folder: Linux x86_64 and arm64, macOS on Apple silicon, Windows. There is no such build for Intel Macs; there the script runs `brew install minisign` when Homebrew is installed.

A bad signature stops the install. When nothing can check it (no connection to GitHub, a temp folder that cannot run programs, an Intel Mac without Homebrew, or `--no-prereqs`), the script says so and continues: the checksums then came over HTTPS from GitHub, just like the script itself, so they protect against a corrupted download but not against a compromised release. Set `BLIRP_REQUIRE_SIGNATURE=1` to refuse to install in that case. Once installed, `blirp update` always verifies the signature itself.

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

Every archive and installer includes `THIRD_PARTY_NOTICES`, the licenses of the Rust crates, JavaScript packages and ConPTY that blirp ships (inside the app bundle on macOS: `blirp.app/Contents/Resources/`; Linux packages: `/usr/lib/blirp/`).

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

Other packaging: every release attaches a rendered Homebrew formula (the CLI only) and winget manifests; they are not published in a tap or `winget-pkgs` yet. There is no Homebrew cask for the desktop app: since 2026-09-01 Homebrew no longer supports casks that fail Gatekeeper, as an app that is not signed and notarized does, and `brew` can no longer skip the quarantine. On macOS, install the app with `install.sh`.

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
blirp --version           # the version you run
blirp update --check      # exit 0: up to date, 10: an update is available
blirp update              # install the latest release
blirp update --version 0.3.1   # a specific release, also older ones
```

The web UI and the desktop app show the running version as the tooltip of the blirp logo in the top bar and under the Settings sections.

`blirp update` downloads `SHA256SUMS.txt`, verifies its signature with the release key built into blirp, downloads the CLI archive (and the desktop app, if the install script installed it), and checks both against the sums. Only then does it stop the daemon (running sessions end as Detached and can be resumed), replace the files and start the daemon again (through the autostart service when one is installed). If replacing any file fails, the files already replaced are put back and the old version starts again. On Windows the running `blirp.exe` is renamed to `blirp.exe.old` and removed the next time blirp starts. It never downgrades unless you pass `--version`, and it only updates installs made by the install scripts; otherwise it tells you how that copy was installed. It prints the version before and after (`Updating blirp 0.1.0 -> 0.1.1`, `Updated blirp 0.1.0 -> 0.1.1`) and records every install attempt in `logs/update.log` in the data folder.

### Update from the app

When a newer release exists, the Settings button in the top bar gets a dot and a banner says **blirp X.Y.Z is available** (dismiss it and it stays hidden until the next release). The daemon asks GitHub at most once a day; **Settings > About** has **Check now** to ask right away (at most once a minute).

- Script installs: **Update now** (in the banner and in Settings > About) runs `blirp update` in the background. It works like the command above: the daemon stops (running sessions end and can be resumed), blirp and the desktop app are replaced, and the daemon starts again, through the autostart service when one is installed. The page reloads once blirp is back. The desktop app signs its window in again by itself; a browser tab shows **Sign in required**, so run `blirp open` to sign it in again (every daemon start issues a new sign-in token). If the update fails, the old version keeps running and Settings > About shows the error. A desktop app that was open during the update still runs its old version; the banner then says **blirp was updated** with **Restart app**, which relaunches it (sessions keep running). A browser page loaded before the update offers **Reload**.
- Installers and packages: **How to update** opens the release page; update the way you installed (see below).
- Only this machine's own desktop app or browser tab can update it. A phone or another device on the LAN portal sees the notice but not the button.

Turn the checks off with `[update] check = false` ([configuration.md](configuration.md#update)). The desktop app has no updater of its own; `blirp update` and **Update now** replace it.

The database migrates forward automatically on the first start of a newer version.

## Uninstalling

```sh
blirp uninstall           # keeps your data in ~/.blirp
blirp uninstall --purge   # also deletes ~/.blirp (asks first; --yes skips the question)
```

It stops the daemon, removes the autostart entry, removes blirp's global agent hooks and MCP entries (only blirp's own entries; everything else in those files stays), then deletes what the install script installed: the CLI, the desktop app, its menu entry or Start Menu shortcut and `blirp://` registration, the `PATH` line or user `Path` entry the script added, and the receipt. It only deletes known file names in the recorded locations; a `blirp` that the scripts did not install (a source build, a package) is left in place. On Windows `blirp.exe` itself is deleted right after the command exits.

Installed with a classic installer instead: `blirp service uninstall` and `blirp hooks uninstall`, quit blirp from the tray, then remove it with the OS (Apps & features; move `blirp.app` to the Trash; `sudo apt remove blirp`).

## Troubleshooting

See [troubleshooting.md](troubleshooting.md): `blirp` not found after installing (PATH), `GLIBC_2.xx not found`, the Windows app not opening (WebView2), the AppImage not starting (FUSE), antivirus warnings, Windows Firewall, "blirp is damaged", the daemon does not start, agents not detected.
