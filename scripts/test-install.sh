#!/usr/bin/env sh
# End-to-end test of the install scripts against a fake release served from
# 127.0.0.1: install.sh on macOS/Linux, install.ps1 (PowerShell 7 and Windows
# PowerShell) from Git Bash on Windows.
#
#   scripts/test-install.sh <blirp binary> <minisign binary>
#
# It packs the binary like the release workflow does, signs SHA256SUMS.txt
# with a throwaway minisign key (the scripts under test are copies with that
# public key swapped in), serves the GitHub-style release JSON and assets
# with python3 -m http.server through BLIRP_RELEASE_BASE_URL, and checks:
#   1. a CLI-only install into a temp home verifies the signature and gives a
#      working `blirp --version`, and installs no prerequisite (package
#      managers on PATH are fakes that fail the test when they run);
#   1b. prerequisites, with PATH cut down to links to the tools the script
#      uses: BLIRP_NO_PREREQS=1 with tar missing fails without a package
#      manager running; (Linux) a missing tar is installed through fake
#      sudo/apt-get, and an install with the app and a display runs no
#      package manager when (a fake) ldconfig lists FUSE; (Linux, Windows) without minisign or OpenSSL,
#      BLIRP_NO_PREREQS=1 + BLIRP_REQUIRE_SIGNATURE=1 refuses without
#      fetching anything, and without the opt-out the pinned minisign is
#      fetched from GitHub and checks the signature (needs network);
#   2. an asset that does not match SHA256SUMS.txt is refused;
#   3. a SHA256SUMS.txt that does not match its signature is refused;
#   4. (Linux) `install.sh --hub` twice: hub role, invite, LAN discovery off,
#      `blirp doctor` server lines, `blirp backup`; without systemd (faked
#      with failing systemctl/loginctl) it falls back to a direct daemon, and
#      with BLIRP_TEST_SYSTEMD=1 it uses the real systemd --user manager (CI:
#      needs linger; installs and removes a unit in the real home and a
#      folder in ~/.cache).
# Apart from 4 with BLIRP_TEST_SYSTEMD=1, nothing outside a temp dir is
# touched: HOME (USERPROFILE, APPDATA, LOCALAPPDATA) points into it, no
# service, PATH entry or desktop app is installed, and
# BLIRP_REQUIRE_SIGNATURE=1 makes a skipped check a failure.
set -eu

if [ $# -ne 2 ]; then
  echo "usage: $0 <blirp binary> <minisign binary>" >&2
  exit 2
fi
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
bin=$1
minisign=$2
[ -f "$bin" ] || { echo "no such file: $bin" >&2; exit 2; }
[ -f "$minisign" ] || { echo "no such file: $minisign" >&2; exit 2; }

case $(uname -m) in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) echo "unsupported CPU: $(uname -m)" >&2; exit 2 ;;
esac
case $(uname -s) in
  Linux) kind=unix; triple=$arch-unknown-linux-gnu ;;
  Darwin) kind=unix; triple=$arch-apple-darwin ;;
  MINGW* | MSYS* | CYGWIN*) kind=windows; triple=x86_64-pc-windows-msvc ;;
  *) echo "unsupported OS: $(uname -s)" >&2; exit 2 ;;
esac
for p in python3 python; do
  if "$p" -c 'import sys; sys.exit(sys.version_info[0] != 3)' >/dev/null 2>&1; then python=$p; break; fi
done
[ -n "${python:-}" ] || { echo "python 3 is required" >&2; exit 2; }
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
else
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
fi
# Paths handed to Windows programs.
winpath() { if [ "$kind" = windows ]; then cygpath -w "$1"; else printf '%s\n' "$1"; fi; }

v=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\([^"]*\)".*/\1/p' "$root/Cargo.toml")
[ -n "$v" ] || { echo "no workspace version in Cargo.toml" >&2; exit 2; }

work=$(mktemp -d 2>/dev/null || mktemp -d -t blirp-install-test)
server=
hub_homes=
sd_dir=${HOME:-/nonexistent}/.cache/blirp-install-test.$$
cleanup() {
  # Daemons of the hub tests (defined below; nothing to stop before that).
  if [ -n "$hub_homes" ]; then stop_hubs; fi
  rm -rf "$sd_dir"
  if [ -n "$server" ]; then
    kill "$server" 2>/dev/null || true
    # It serves from inside $work; let it exit before the rm.
    wait "$server" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

fail() {
  echo "FAIL: $1" >&2
  if [ -f "$work/out.log" ]; then sed 's/^/  | /' "$work/out.log" >&2; fi
  exit 1
}

# ---------------------------------------------------------------- release

www=$work/www
dl=$www/download
mkdir -p "$dl" "$www/releases/tags" "$work/stage"
name=blirp-$v-$triple
mkdir -p "$work/stage/$name"
cp "$root/LICENSE" "$work/stage/$name/"
if [ "$kind" = windows ]; then
  asset=$name.zip
  cp "$bin" "$work/stage/$name/blirp.exe"
  # Stand-ins: install.ps1 only checks that they are there.
  mkdir -p "$work/stage/$name/x64"
  printf 'stand-in\n' >"$work/stage/$name/conpty.dll"
  printf 'stand-in\n' >"$work/stage/$name/x64/OpenConsole.exe"
  "$python" -c 'import shutil, sys; shutil.make_archive(sys.argv[1], "zip", sys.argv[2], sys.argv[3])' \
    "$(winpath "$dl/$name")" "$(winpath "$work/stage")" "$name"
else
  asset=$name.tar.gz
  cp "$bin" "$work/stage/$name/blirp"
  chmod 0755 "$work/stage/$name/blirp"
  tar -czf "$dl/$asset" -C "$work/stage" "$name"
fi

write_sums() { printf '%s  %s\n' "$(sha256 "$dl/$asset")" "$asset" >"$dl/SHA256SUMS.txt"; }
write_sums
"$minisign" -G -W -p "$(winpath "$work/test.pub")" -s "$(winpath "$work/test.key")" >/dev/null
"$minisign" -S -s "$(winpath "$work/test.key")" -m "$(winpath "$dl/SHA256SUMS.txt")" \
  -x "$(winpath "$dl/SHA256SUMS.txt.sig")" >/dev/null
pubkey=$(sed -n 2p "$work/test.pub" | tr -d '\r')
case $pubkey in RW*) ;; *) fail "unexpected minisign public key file" ;; esac

# The scripts under test, trusting the throwaway key instead of the release key.
if [ "$kind" = windows ]; then
  script=$work/install.ps1
  sed "s|^\( *\\\$ReleasePubkey = \)'[^']*'|\1'$pubkey'|" "$root/install.ps1" >"$script"
else
  script=$work/install.sh
  sed "s|^RELEASE_PUBKEY=.*|RELEASE_PUBKEY=$pubkey|" "$root/install.sh" >"$script"
fi
grep -F "$pubkey" "$script" >/dev/null || fail "could not swap the test key into $(basename "$script")"

# Serve $www on a free port. A plain TCPServer, not `python3 -m http.server`:
# HTTPServer resolves its own name (getfqdn) before it announces the port,
# which can take tens of seconds on macOS runners.
(cd "$www" && exec "$python" -u -c '
import http.server, socketserver
class Handler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass
socketserver.TCPServer.allow_reuse_address = True
with socketserver.ThreadingTCPServer(("127.0.0.1", 0), Handler) as httpd:
    print("port", httpd.server_address[1])
    httpd.serve_forever()
') >"$work/server.log" 2>&1 &
server=$!
port=
i=0
while [ $i -lt 300 ]; do
  port=$(tr -d '\r' <"$work/server.log" | sed -n 's/^port \([0-9][0-9]*\)$/\1/p' | head -n 1)
  [ -n "$port" ] && break
  sleep 0.1
  i=$((i + 1))
done
[ -n "$port" ] || { cat "$work/server.log" >&2; fail "the test server did not start"; }
origin=http://127.0.0.1:$port
base=$origin/releases

n=0
entries=
for f in "$asset" SHA256SUMS.txt SHA256SUMS.txt.sig; do
  n=$((n + 1))
  entries="$entries${entries:+,}{\"name\":\"$f\",\"url\":\"$base/assets/$n\",\"browser_download_url\":\"$origin/download/$f\"}"
done
printf '{"tag_name":"v%s","html_url":"%s/tag/v%s","assets":[%s]}\n' "$v" "$origin" "$v" "$entries" >"$www/releases/latest"
cp "$www/releases/latest" "$www/releases/tags/v$v"

# ----------------------------------------------------------------- runner

# run_install HOME_DIR SHELL [--version]: run the script under test into a fresh
# home; its output goes to $work/out.log.
run_install() {
  _home=$1
  mkdir -p "$_home"
  # The shell by absolute path: run_path may not contain it.
  _shp=$(command -v "$2")
  if [ "$kind" = windows ]; then
    _wh=$(cygpath -w "$_home")
    _ver=
    if [ "${3:-}" = --version ]; then _ver=$v; fi
    env -u GITHUB_TOKEN \
      USERPROFILE="$_wh" APPDATA="$_wh\\AppData\\Roaming" LOCALAPPDATA="$_wh\\AppData\\Local" \
      BLIRP_INSTALL_DIR="$_wh\\Programs\\blirp" BLIRP_HOME="$_wh\\.blirp" \
      BLIRP_RELEASE_BASE_URL="$base" BLIRP_REQUIRE_SIGNATURE=1 BLIRP_NO_APP=1 \
      BLIRP_NO_MODIFY_PATH=1 BLIRP_VERSION="$_ver" BLIRP_NO_PREREQS="$no_prereqs" PATH="${run_path:-$PATH}" \
      "$_shp" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$(cygpath -w "$script")" >"$work/out.log" 2>&1
  else
    _args=--no-app
    if [ "${3:-}" = --version ]; then _args="--no-app --version $v"; fi
    # With the app (and a display, so FUSE is checked).
    _disp=${DISPLAY:-}
    if [ -n "$run_app" ]; then
      _args=
      _disp=:0
    fi
    # shellcheck disable=SC2086 # option words
    env -u GITHUB_TOKEN -u BLIRP_INSTALL_DIR -u XDG_DATA_HOME -u BLIRP_VERSION \
      HOME="$_home" SHELL=/bin/sh BLIRP_HOME="$_home/.blirp" \
      BLIRP_RELEASE_BASE_URL="$base" BLIRP_REQUIRE_SIGNATURE=1 \
      BLIRP_NO_PREREQS="$no_prereqs" PATH="${run_path:-$work/pm-fail:$PATH}" \
      DISPLAY="$_disp" \
      "$_shp" "$script" $_args >"$work/out.log" 2>&1
  fi
}
# Overrides for the next run_install: PATH, BLIRP_NO_PREREQS, and (unix)
# with the desktop app and a display.
run_path=
no_prereqs=
run_app=

# Package managers that must not run when nothing is missing: they only log.
mkdir -p "$work/pm-fail"
for t in apt-get dnf pacman zypper brew sudo; do
  printf '#!/bin/sh\nprintf "%%s %%s\\n" %s "$*" >>"%s"\nexit 1\n' "$t" "$work/pm.log" >"$work/pm-fail/$t"
  chmod +x "$work/pm-fail/$t"
done
no_install_of_prereqs() {
  [ ! -e "$work/pm.log" ] || fail "$1: a package manager ran: $(cat "$work/pm.log")"
  if grep -E 'installing missing prerequisites|downloading minisign|installing the Microsoft Edge' "$work/out.log" >/dev/null; then
    fail "$1: prerequisites were installed although none was missing"
  fi
}

installed_bin() {
  if [ "$kind" = windows ]; then
    printf '%s\n' "$1/Programs/blirp/blirp.exe"
  else
    printf '%s\n' "$1/.local/bin/blirp"
  fi
}
receipt() {
  if [ "$kind" = windows ]; then
    printf '%s\n' "$1/Programs/blirp/install.json"
  else
    printf '%s\n' "$1/.local/share/blirp/install.json"
  fi
}

if [ "$kind" = windows ]; then
  shells=
  for s in pwsh powershell; do
    if command -v "$s" >/dev/null 2>&1; then shells="$shells $s"; fi
  done
  [ -n "$shells" ] || fail "neither pwsh nor powershell found"
else
  shells="sh"
fi

# ------------------------------------------------------------------ tests

for sh_ in $shells; do
  h=$work/home-ok-$sh_
  run_install "$h" "$sh_" || fail "$sh_: install failed"
  grep -F 'release signature verified' "$work/out.log" >/dev/null || fail "$sh_: the signature was not checked"
  b=$(installed_bin "$h")
  [ -f "$b" ] || fail "$sh_: $b missing"
  out=$(HOME="$h" BLIRP_HOME="$h/.blirp" "$b" --version 2>&1) || fail "$sh_: blirp --version failed: $out"
  case $out in
    *" $v"*) ;;
    *) fail "$sh_: blirp --version printed '$out', expected version $v" ;;
  esac
  grep -E "\"version\": +\"$v\"" "$(receipt "$h")" >/dev/null || fail "$sh_: install receipt missing or wrong"
  no_install_of_prereqs "$sh_"
  echo "ok: $sh_ installs blirp $v with a verified signature ($out)"
done
sh_=${shells##* }
echo "ok: nothing is installed when every prerequisite is present"

# ---------------------------------------------------------- prerequisites

# min_path DIR TOOL...: DIR with links to just these tools (those that exist).
min_path() {
  _d=$1
  shift
  mkdir -p "$_d"
  for _t in "$@"; do
    _p=$(command -v "$_t" 2>/dev/null) || continue
    case $_p in /*) ln -s "$_p" "$_d/$_t" ;; esac
  done
}
if [ "$kind" = unix ]; then
  # What install.sh runs, minus tar and any signature checker.
  min_path "$work/min" sh uname id mktemp sha256sum shasum curl awk tr sed cut dd od wc \
    cp chmod mv mkdir rm basename dirname grep cat gzip env sysctl unzip
  min_path "$work/ossl" openssl
  # A missing tool with the package manager turned off: a clear error, and
  # no package manager runs.
  run_path=$work/pm-fail:$work/min:$work/ossl no_prereqs=1
  if run_install "$work/home-no-prereqs" "$sh_"; then fail "BLIRP_NO_PREREQS=1: installed without tar"; fi
  run_path='' no_prereqs=''
  grep -F 'tar is required' "$work/out.log" >/dev/null || fail "BLIRP_NO_PREREQS=1: expected 'tar is required'"
  [ ! -e "$work/pm.log" ] || fail "BLIRP_NO_PREREQS=1: a package manager ran: $(cat "$work/pm.log")"
  echo "ok: BLIRP_NO_PREREQS=1 installs no missing tool"
fi
if [ "$(uname -s)" = Linux ]; then
  # A missing tar is installed through sudo and apt-get (fakes: sudo runs
  # the command, apt-get logs it and links the real tar).
  mkdir -p "$work/pm-apt"
  # shellcheck disable=SC2016 # expanded when the fakes run
  printf '#!/bin/sh\nprintf "sudo %%s\\n" "$*" >>"%s"\n[ "$1" != -n ] || shift\nexec "$@"\n' \
    "$work/pm.log" >"$work/pm-apt/sudo"
  printf '#!/bin/sh\nprintf "apt-get %%s\\n" "$*" >>"%s"\ncase " $* " in *" install "*" tar "*) "%s" -s "%s" "%s/tar" ;; esac\n' \
    "$work/pm.log" "$(command -v ln)" "$(command -v tar)" "$work/min" >"$work/pm-apt/apt-get"
  chmod +x "$work/pm-apt/sudo" "$work/pm-apt/apt-get"
  run_path=$work/pm-apt:$work/min:$work/ossl
  run_install "$work/home-prereqs" "$sh_" || fail "prerequisites: install with a missing tar failed"
  run_path=
  grep -F 'installing missing prerequisites:' "$work/out.log" | grep -F 'apt-get install -y -q tar' >/dev/null ||
    fail "prerequisites: the apt-get command was not printed"
  grep -E '^apt-get .*install -y -q tar$' "$work/pm.log" >/dev/null || fail "prerequisites: apt-get did not install tar"
  [ -x "$(installed_bin "$work/home-prereqs")" ] || fail "prerequisites: blirp missing"
  rm -f "$work/pm.log" "$work/min/tar"
  echo "ok: a missing tar is installed with the package manager"
  ln -s "$(command -v tar)" "$work/min/tar"
  # With the app and a display, FUSE present (fake ldconfig): no package manager.
  mkdir -p "$work/fuse"
  printf '#!/bin/sh\nprintf "\\tlibfuse.so.2 (libc6,x86-64) => /lib/x86_64-linux-gnu/libfuse.so.2\\n"\n' >"$work/fuse/ldconfig"
  chmod +x "$work/fuse/ldconfig"
  run_path=$work/pm-fail:$work/fuse:$PATH run_app=1
  run_install "$work/home-app" "$sh_" || fail "FUSE present: install with the app failed"
  run_path='' run_app=''
  no_install_of_prereqs "FUSE present"
  echo "ok: with FUSE present an install with the app installs nothing"
fi
if [ "$kind" = windows ]; then
  # No Git (its OpenSSL) and no minisign on Path.
  run_path="$(cygpath -u "$SYSTEMROOT")/System32:$(cygpath -u "$SYSTEMROOT"):$(cygpath -u "$SYSTEMROOT")/System32/WindowsPowerShell/v1.0"
elif [ "$(uname -s)" = Linux ]; then
  run_path=$work/pm-fail:$work/min
else
  # macOS: install.sh also finds Homebrew's OpenSSL off PATH.
  run_path=
fi
if [ -n "$run_path" ]; then
  # Nothing can check the signature and fetching minisign is turned off:
  # BLIRP_REQUIRE_SIGNATURE=1 refuses, and nothing is downloaded.
  no_prereqs=1
  if run_install "$work/home-no-verifier" "$sh_"; then fail "no verifier: installed with BLIRP_REQUIRE_SIGNATURE=1"; fi
  no_prereqs=
  grep -F 'BLIRP_REQUIRE_SIGNATURE is set' "$work/out.log" >/dev/null || fail "no verifier: expected the BLIRP_REQUIRE_SIGNATURE error"
  if grep -F 'downloading minisign' "$work/out.log" >/dev/null; then fail "no verifier: minisign fetched despite BLIRP_NO_PREREQS=1"; fi
  echo "ok: BLIRP_NO_PREREQS=1 fetches no minisign"
  # Otherwise the pinned minisign release is fetched from GitHub for the check.
  run_install "$work/home-fetched-minisign" "$sh_" || fail "no verifier: install with a fetched minisign failed"
  run_path=
  grep -F 'downloading minisign 0.12' "$work/out.log" >/dev/null || fail "no verifier: minisign was not fetched"
  grep -F 'release signature verified (minisign)' "$work/out.log" >/dev/null || fail "no verifier: not verified with the fetched minisign"
  echo "ok: without minisign or OpenSSL the pinned minisign checks the signature"
fi

# ------------------------------------------------------------ hub (Linux)

# `install.sh --hub`, twice, as on a fresh server. Mode `fallback`: systemctl
# and loginctl fail as in a container, so hub setup must say so and start the
# daemon directly. Mode `systemd` (BLIRP_TEST_SYSTEMD=1, CI only): the real
# systemd --user manager of the current user, which needs linger already on;
# it writes ~/.config/systemd/user/blirp.service in the real home and removes
# it again. Every daemon is loopback-only and picks a free port.
# The systemd mode installs the CLI under ~/.cache ($sd_dir), not the temp
# dir: service install refuses a binary under /tmp, which is gone at reboot.
# XDG_CONFIG_HOME is unset: runners set it to the real home's .config, where
# the fallback mode must not look for a unit.
hub_mode_env() {
  # Printed as NAME=VALUE words for `env`.
  if [ "$1" = systemd ]; then
    printf 'HOME=%s XDG_DATA_HOME=%s BLIRP_INSTALL_DIR=%s\n' "$HOME" "$sd_dir/share" "$sd_dir/bin"
  else
    printf 'HOME=%s PATH=%s\n' "$2" "$work/shim:$PATH"
  fi
}
hub_bin() {
  if [ "$1" = systemd ]; then printf '%s\n' "$sd_dir/bin/blirp"; else installed_bin "$2"; fi
}
# hub_cli MODE HOME ARGS...: the installed blirp in that setup's environment.
hub_cli() {
  _m=$1
  _h=$2
  shift 2
  # shellcheck disable=SC2046 # NAME=VALUE words without spaces
  env -u XDG_CONFIG_HOME $(hub_mode_env "$_m" "$_h") BLIRP_HOME="$_h/.blirp" BLIRP_LOOPBACK_ONLY=1 \
    "$(hub_bin "$_m" "$_h")" "$@"
}
run_hub() {
  # shellcheck disable=SC2046 # NAME=VALUE words without spaces
  env -u GITHUB_TOKEN -u BLIRP_INSTALL_DIR -u XDG_DATA_HOME -u XDG_CONFIG_HOME -u BLIRP_VERSION \
    $(hub_mode_env "$1" "$2") SHELL=/bin/sh BLIRP_HOME="$2/.blirp" BLIRP_LOOPBACK_ONLY=1 \
    BLIRP_RELEASE_BASE_URL="$base" BLIRP_REQUIRE_SIGNATURE=1 \
    sh "$script" --hub >"$work/out.log" 2>&1
}
stop_hubs() {
  for _mh in $hub_homes; do
    _m=${_mh%%:*}
    _h=${_mh#*:}
    [ -x "$(hub_bin "$_m" "$_h")" ] || continue
    hub_cli "$_m" "$_h" stop >/dev/null 2>&1 || true
    if [ "$_m" = systemd ]; then
      hub_cli "$_m" "$_h" service uninstall >/dev/null 2>&1 || true
      XDG_RUNTIME_DIR=/run/user/$(id -u) systemctl --user unset-environment BLIRP_LOOPBACK_ONLY         >/dev/null 2>&1 || true
    fi
  done
}

hub_test() {
  mode=$1
  h=$work/hub-$mode
  mkdir -p "$h/.blirp"
  # A free port: a runner may already use the default one.
  printf '[daemon]\nport = 0\n' >"$h/.blirp/config.toml"
  hub_homes="$hub_homes $mode:$h"
  if [ "$mode" = systemd ]; then
    [ -S "/run/user/$(id -u)/bus" ] || fail "systemd: no user manager (run: sudo loginctl enable-linger $(id -un))"
    # The service's daemon inherits the manager's environment.
    systemctl --user set-environment BLIRP_LOOPBACK_ONLY=1 ||
      XDG_RUNTIME_DIR=/run/user/$(id -u) systemctl --user set-environment BLIRP_LOOPBACK_ONLY=1 ||
      fail "systemd: cannot set the user manager's environment"
  else
    mkdir -p "$work/shim"
    for t in systemctl loginctl; do
      printf '#!/bin/sh\necho "System has not been booted with systemd as init system (PID 1)." >&2\nexit 1\n' >"$work/shim/$t"
      chmod +x "$work/shim/$t"
    done
  fi

  run_hub "$mode" "$h" || fail "hub/$mode: install.sh --hub failed"
  grep -E 'blirp pair blirp1-[a-z0-9]+ [A-Z0-9]{4}-[A-Z0-9]{4}' "$work/out.log" >/dev/null ||
    fail "hub/$mode: no invite printed"
  grep -F 'LAN discovery (mDNS) turned off' "$work/out.log" >/dev/null ||
    fail "hub/$mode: LAN discovery was not turned off"
  grep -E '^lan_discovery = false' "$h/.blirp/config.toml" >/dev/null ||
    fail "hub/$mode: config.toml does not keep LAN discovery off"
  grep -E '^keep_awake = false' "$h/.blirp/config.toml" >/dev/null ||
    fail "hub/$mode: keep-awake was not turned off"
  grep -F 'blirp agents set-token claude' "$work/out.log" >/dev/null || fail "hub/$mode: no next steps"
  if [ "$mode" = systemd ]; then
    grep -F 'Linger is on' "$work/out.log" >/dev/null || fail "hub/$mode: linger not reported on"
    grep -F 'Autostart installed' "$work/out.log" >/dev/null || fail "hub/$mode: autostart not installed"
    XDG_RUNTIME_DIR=/run/user/$(id -u) systemctl --user is-active --quiet blirp.service ||
      fail "hub/$mode: blirp.service is not active"
  else
    grep -F 'no autostart service was installed' "$work/out.log" >/dev/null ||
      grep -F 'systemd is not running here' "$work/out.log" >/dev/null ||
      fail "hub/$mode: no fallback message without systemd"
  fi
  st=$(hub_cli "$mode" "$h" hub status 2>&1) || fail "hub/$mode: blirp hub status failed: $st"
  case $st in *"role       hub"*) ;; *) fail "hub/$mode: not a hub: $st" ;; esac
  id1=$(printf '%s\n' "$st" | sed -n 's/^machine *//p')

  hub_cli "$mode" "$h" doctor >"$work/out.log" 2>&1 || fail "hub/$mode: blirp doctor failed"
  for line in '[info] role: hub' 'autostart:' 'linger:' '[info] relay: off (BLIRP_LOOPBACK_ONLY=1)'; do
    grep -F "$line" "$work/out.log" >/dev/null || fail "hub/$mode: doctor has no '$line' line"
  done
  if [ "$mode" = systemd ]; then
    grep -F '[info] linger: on' "$work/out.log" >/dev/null || fail "hub/$mode: doctor: linger not on"
  fi

  # Again: an upgrade in place that keeps the hub and prints a new invite.
  run_hub "$mode" "$h" || fail "hub/$mode: second install.sh --hub failed"
  grep -E 'blirp pair blirp1-' "$work/out.log" >/dev/null || fail "hub/$mode: no invite on the second run"
  if grep -F 'LAN discovery (mDNS) turned' "$work/out.log" >/dev/null; then
    fail "hub/$mode: the second run changed LAN discovery"
  fi
  if [ "$mode" = systemd ]; then
    # The upgrade stopped the daemon; the unchanged unit starts it again.
    XDG_RUNTIME_DIR=/run/user/$(id -u) systemctl --user is-active --quiet blirp.service ||
      fail "hub/$mode: blirp.service is not active after the second run"
  fi
  st=$(hub_cli "$mode" "$h" hub status 2>&1) || fail "hub/$mode: blirp hub status failed: $st"
  [ "$(printf '%s\n' "$st" | sed -n 's/^machine *//p')" = "$id1" ] || fail "hub/$mode: the machine id changed"

  if [ "$mode" = systemd ]; then
    # A daemon started directly (e.g. before linger was on) is handed over
    # to the service by `blirp hub setup`.
    XDG_RUNTIME_DIR=/run/user/$(id -u) systemctl --user stop blirp.service ||
      fail "hub/$mode: cannot stop blirp.service"
    hub_cli "$mode" "$h" daemon --detach >"$work/out.log" 2>&1 || fail "hub/$mode: blirp daemon --detach failed"
    hub_cli "$mode" "$h" hub setup >"$work/out.log" 2>&1 || fail "hub/$mode: blirp hub setup failed"
    grep -F 'Handing the running daemon over to the service' "$work/out.log" >/dev/null ||
      fail "hub/$mode: the direct daemon was not handed over"
    XDG_RUNTIME_DIR=/run/user/$(id -u) systemctl --user is-active --quiet blirp.service ||
      fail "hub/$mode: blirp.service is not active after the takeover"
  fi

  # A backup of the live database, never over an existing file.
  hub_cli "$mode" "$h" backup "$h/backup.db" >"$work/out.log" 2>&1 || fail "hub/$mode: blirp backup failed"
  [ -s "$h/backup.db" ] || fail "hub/$mode: the backup is empty"
  if hub_cli "$mode" "$h" backup "$h/backup.db" >"$work/out.log" 2>&1; then
    fail "hub/$mode: blirp backup overwrote a file"
  fi
  echo "ok: install.sh --hub ($mode) sets up a hub, prints an invite, and runs again cleanly"
}

if [ "$kind" = unix ] && [ "$(uname -s)" = Linux ]; then
  hub_test fallback
  if [ "${BLIRP_TEST_SYSTEMD:-}" = 1 ]; then hub_test systemd; fi
  stop_hubs
  hub_homes=
fi

# An asset that does not match SHA256SUMS.txt.
printf 'tampered' >>"$dl/$asset"
h=$work/home-bad-asset
if run_install "$h" "$sh_" --version; then fail "a tampered $asset was installed"; fi
grep -F 'checksum mismatch' "$work/out.log" >/dev/null || fail "tampered asset: expected a checksum mismatch"
[ ! -e "$(installed_bin "$h")" ] || fail "tampered asset: blirp was installed anyway"
echo "ok: a tampered asset is refused"

# Checksums that match the tampered asset but not the signature.
write_sums
h=$work/home-bad-sums
if run_install "$h" "$sh_" --version; then fail "SHA256SUMS.txt with a bad signature was accepted"; fi
grep -F 'is not signed by the blirp release key' "$work/out.log" >/dev/null ||
  fail "tampered SHA256SUMS.txt: expected a signature failure"
[ ! -e "$(installed_bin "$h")" ] || fail "tampered SHA256SUMS.txt: blirp was installed anyway"
echo "ok: SHA256SUMS.txt that does not match its signature is refused"
