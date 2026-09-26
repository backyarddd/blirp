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
#      working `blirp --version`;
#   2. an asset that does not match SHA256SUMS.txt is refused;
#   3. a SHA256SUMS.txt that does not match its signature is refused.
# Nothing outside a temp dir is touched: HOME (USERPROFILE, APPDATA,
# LOCALAPPDATA) points into it, no service, PATH entry or desktop app is
# installed, and BLIRP_REQUIRE_SIGNATURE=1 makes a skipped check a failure.
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
cleanup() {
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
  if [ "$kind" = windows ]; then
    _wh=$(cygpath -w "$_home")
    _ver=
    if [ "${3:-}" = --version ]; then _ver=$v; fi
    env -u GITHUB_TOKEN \
      USERPROFILE="$_wh" APPDATA="$_wh\\AppData\\Roaming" LOCALAPPDATA="$_wh\\AppData\\Local" \
      BLIRP_INSTALL_DIR="$_wh\\Programs\\blirp" BLIRP_HOME="$_wh\\.blirp" \
      BLIRP_RELEASE_BASE_URL="$base" BLIRP_REQUIRE_SIGNATURE=1 BLIRP_NO_APP=1 \
      BLIRP_NO_MODIFY_PATH=1 BLIRP_VERSION="$_ver" \
      "$2" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$(cygpath -w "$script")" >"$work/out.log" 2>&1
  else
    _args=--no-app
    if [ "${3:-}" = --version ]; then _args="--no-app --version $v"; fi
    # shellcheck disable=SC2086 # option words
    env -u GITHUB_TOKEN -u BLIRP_INSTALL_DIR -u XDG_DATA_HOME -u BLIRP_VERSION \
      HOME="$_home" SHELL=/bin/sh BLIRP_HOME="$_home/.blirp" \
      BLIRP_RELEASE_BASE_URL="$base" BLIRP_REQUIRE_SIGNATURE=1 \
      "$2" "$script" $_args >"$work/out.log" 2>&1
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
  echo "ok: $sh_ installs blirp $v with a verified signature ($out)"
done
sh_=${shells##* }

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
