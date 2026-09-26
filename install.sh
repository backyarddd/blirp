#!/bin/sh
# blirp installer for macOS and Linux (x86_64, arm64).
#
#   curl -fsSL https://raw.githubusercontent.com/backyarddd/blirp/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/backyarddd/blirp/main/install.sh | sh -s -- --service
#
# Installs the `blirp` CLI into ${BLIRP_INSTALL_DIR:-~/.local/bin} and the
# desktop app (macOS ~/Applications/blirp.app, Linux an AppImage in
# ~/.local/share/blirp with a menu entry). Every download is checked against
# the release's SHA256SUMS.txt. Running it again upgrades in place. Options:
# see usage() below or `sh install.sh --help`. Docs: docs/install.md.

set -eu

RELEASES_API=https://api.github.com/repos/backyarddd/blirp/releases
MARKER="# added by the blirp installer"

say() { printf 'blirp: %s\n' "$*"; }
die() {
  printf 'blirp: error: %s\n' "$*" >&2
  exit 1
}
has() { command -v "$1" >/dev/null 2>&1; }

usage() {
  cat <<'EOF'
blirp installer for macOS and Linux.

  curl -fsSL https://raw.githubusercontent.com/backyarddd/blirp/main/install.sh | sh -s -- [options]

Options (or the environment variable in brackets):
  --version X     install release X instead of the latest   [BLIRP_VERSION]
  --no-app        CLI only, no desktop app                   [BLIRP_NO_APP=1]
  --service       start the daemon at login                  [BLIRP_SERVICE=1]
  --modify-path   add the install folder to PATH in your shell startup file
                                                             [BLIRP_MODIFY_PATH=1]
Environment:
  BLIRP_INSTALL_DIR       where the CLI goes (default ~/.local/bin)
  GITHUB_TOKEN            token for a private repository or rate limits
  BLIRP_RELEASE_BASE_URL  releases API to use instead of GitHub (mirrors, tests)
EOF
}

version=${BLIRP_VERSION:-}
no_app=${BLIRP_NO_APP:-}
service=${BLIRP_SERVICE:-}
modify_path=${BLIRP_MODIFY_PATH:-}
while [ $# -gt 0 ]; do
  case $1 in
    --version)
      [ $# -ge 2 ] || die "--version needs a value"
      version=$2
      shift 2
      ;;
    --version=*)
      version=${1#*=}
      shift
      ;;
    --no-app) no_app=1; shift ;;
    --service) service=1; shift ;;
    --modify-path) modify_path=1; shift ;;
    -h | --help)
      usage
      exit 0
      ;;
    *) die "unknown option: $1 (see --help)" ;;
  esac
done
version=${version#v}
# Accept 0/false for the boolean variables.
case $no_app in 0 | false) no_app= ;; esac
case $service in 0 | false) service= ;; esac
case $modify_path in 0 | false) modify_path= ;; esac

# ------------------------------------------------------------------ platform

os=$(uname -s)
arch=$(uname -m)
case $arch in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) die "unsupported CPU: $arch (blirp ships x86_64 and arm64 builds)" ;;
esac
case $os in
  Darwin)
    # An x86_64 shell under Rosetta still gets the native arm64 build.
    if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
      arch=aarch64
    fi
    triple=$arch-apple-darwin
    app_arch=$arch
    [ "$arch" = x86_64 ] && app_arch=x64
    ;;
  Linux)
    triple=$arch-unknown-linux-gnu
    app_arch=$arch
    [ "$arch" = x86_64 ] && app_arch=amd64
    ;;
  *) die "unsupported OS: $os (on Windows use install.ps1)" ;;
esac

[ -n "${HOME:-}" ] || die "HOME is not set"
install_dir=${BLIRP_INSTALL_DIR:-$HOME/.local/bin}
case $install_dir in
  /*) ;;
  *) install_dir=$(pwd)/$install_dir ;;
esac
case ${XDG_DATA_HOME:-} in
  /*) data_home=$XDG_DATA_HOME ;;
  *) data_home=$HOME/.local/share ;;
esac
share_dir=$data_home/blirp
receipt=$share_dir/install.json
if [ "$os" = Darwin ]; then
  app_path=$HOME/Applications/blirp.app
else
  app_path=$share_dir/blirp.AppImage
fi

has tar || die "tar is required"
has mktemp || die "mktemp is required"
if has sha256sum; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif has shasum; then
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die "sha256sum or shasum is required"
fi
if has curl; then
  downloader=curl
elif has wget; then
  downloader=wget
  # wget keeps custom headers across redirects; never send a token that way.
  [ -z "${GITHUB_TOKEN:-}" ] || die "GITHUB_TOKEN needs curl (wget would forward it to the download host)"
else
  die "curl or wget is required"
fi

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t blirp)
stage=
cleanup() {
  rm -rf "$tmp"
  if [ -n "$stage" ]; then rm -rf "$stage"; fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# fetch URL OUT [ACCEPT]
fetch() {
  _url=$1
  _out=$2
  _accept=${3:-}
  if [ "$downloader" = curl ]; then
    # curl sends custom Authorization headers only to the first host.
    set -- curl -fsSL --retry 3 -o "$_out"
    if [ -n "$_accept" ]; then set -- "$@" -H "Accept: $_accept"; fi
    if [ -n "${GITHUB_TOKEN:-}" ]; then set -- "$@" -H "Authorization: Bearer $GITHUB_TOKEN"; fi
    "$@" "$_url"
  else
    set -- wget -q -O "$_out"
    if [ -n "$_accept" ]; then set -- "$@" "--header=Accept: $_accept"; fi
    "$@" "$_url"
  fi
}

# ------------------------------------------------------------------- release

base=${BLIRP_RELEASE_BASE_URL:-$RELEASES_API}
base=${base%/}
if [ -n "$version" ]; then
  release_url=$base/tags/v$version
else
  release_url=$base/latest
fi
if ! fetch "$release_url" "$tmp/release.json" application/vnd.github+json; then
  hint=
  [ -n "${GITHUB_TOKEN:-}" ] || hint=" (private repository? set GITHUB_TOKEN)"
  if [ -n "$version" ]; then
    die "release v$version not found at $base$hint"
  fi
  die "no published release found at $base$hint"
fi

# One JSON member per line, then: the release tag, and per asset its name,
# API URL and browser URL (the API URL serves the bytes with a token).
# shellcheck disable=SC2020 # each of the three characters becomes a newline
tr ',{}' '\n\n\n' <"$tmp/release.json" >"$tmp/release.lines"
tag=$(awk -F'"' '$2 == "tag_name" { print $4; exit }' "$tmp/release.lines")
[ -n "$tag" ] || die "unexpected release data from $release_url"
ver=${tag#v}
awk -F'"' '
  $2 == "url" && $4 ~ /\/releases\/assets\/[0-9]+$/ { api = $4 }
  $2 == "browser_download_url" { n = split($4, p, "/"); print p[n], api, $4 }
' "$tmp/release.lines" >"$tmp/assets"

has_asset() { awk -v n="$1" '$1 == n { f = 1 } END { exit !f }' "$tmp/assets"; }

# get_asset NAME OUT
get_asset() {
  _line=$(awk -v n="$1" '$1 == n { print; exit }' "$tmp/assets")
  [ -n "$_line" ] || die "release $tag has no asset $1"
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    _url=$(printf '%s\n' "$_line" | cut -d' ' -f2)
    fetch "$_url" "$2" application/octet-stream
  else
    _url=$(printf '%s\n' "$_line" | cut -d' ' -f3)
    fetch "$_url" "$2"
  fi
}

get_asset SHA256SUMS.txt "$tmp/SHA256SUMS.txt" || die "download of SHA256SUMS.txt failed"

# verified NAME OUT: download and check against SHA256SUMS.txt
verified() {
  get_asset "$1" "$2" || die "download of $1 failed"
  _want=$(awk -v n="$1" '{ f = $2; sub(/^\*/, "", f) } f == n { print tolower($1); exit }' "$tmp/SHA256SUMS.txt")
  [ -n "$_want" ] || die "$1 is not listed in SHA256SUMS.txt"
  _got=$(sha256 "$2" | tr 'A-F' 'a-f')
  [ "$_want" = "$_got" ] || die "checksum mismatch for $1 (expected $_want, got $_got)"
}

cli_name=blirp-$ver-$triple
say "installing blirp $ver ($triple)"
has_asset "$cli_name.tar.gz" || die "release $tag has no build for $triple"
verified "$cli_name.tar.gz" "$tmp/cli.tar.gz"
mkdir -p "$tmp/cli"
tar -xzf "$tmp/cli.tar.gz" -C "$tmp/cli"
[ -f "$tmp/cli/$cli_name/blirp" ] || die "$cli_name.tar.gz does not contain $cli_name/blirp"

if [ "$os" = Darwin ]; then
  app_asset=blirp_${ver}_$app_arch.app.tar.gz
else
  app_asset=blirp_${ver}_$app_arch.AppImage
fi
# Everything is downloaded and verified before anything is installed.
app_ready=
if [ -z "$no_app" ]; then
  if has_asset "$app_asset"; then
    verified "$app_asset" "$tmp/app.download"
    app_ready=1
  else
    say "warning: release $tag has no desktop app for this platform ($app_asset); installing the CLI only"
  fi
fi

# ------------------------------------------------------------------- install

bin=$install_dir/blirp
was_running=
if [ -x "$bin" ] && "$bin" status >/dev/null 2>&1; then
  was_running=1
  say "stopping the running daemon for the upgrade"
  "$bin" stop >/dev/null || die "could not stop the running daemon (blirp stop)"
fi

mkdir -p "$install_dir"
# Same folder, then rename: atomic, and safe while the old binary runs.
cp "$tmp/cli/$cli_name/blirp" "$install_dir/.blirp.new.$$"
chmod 0755 "$install_dir/.blirp.new.$$"
mv -f "$install_dir/.blirp.new.$$" "$bin"
say "installed $bin"

# Exec value of a desktop entry: quoted, with \ " ` $ escaped.
desktop_quote() {
  printf '"%s"' "$(printf '%s' "$1" | sed 's/[\\"`$]/\\&/g')"
}

install_app() {
  if [ "$os" = Darwin ]; then
    mkdir -p "$HOME/Applications"
    stage=$HOME/Applications/.blirp-install.$$
    mkdir -p "$stage"
    tar -xzf "$tmp/app.download" -C "$stage"
    [ -d "$stage/blirp.app" ] || die "$app_asset does not contain blirp.app"
    rm -rf "$app_path"
    mv "$stage/blirp.app" "$app_path"
    rm -rf "$stage"
    stage=
  else
    mkdir -p "$share_dir" "$data_home/applications"
    cp "$tmp/app.download" "$share_dir/.blirp.AppImage.new.$$"
    chmod 0755 "$share_dir/.blirp.AppImage.new.$$"
    mv -f "$share_dir/.blirp.AppImage.new.$$" "$app_path"
    cat >"$data_home/applications/blirp.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=blirp
Comment=Workspace and memory for CLI coding agents
Exec=$(desktop_quote "$app_path") %u
Terminal=false
Categories=Development;
MimeType=x-scheme-handler/blirp;
StartupWMClass=blirp
EOF
    if has update-desktop-database; then
      update-desktop-database "$data_home/applications" >/dev/null 2>&1 || true
    fi
  fi
  say "installed the desktop app: $app_path"
}

if [ -n "$app_ready" ]; then install_app; fi
app_recorded=
if [ -e "$app_path" ]; then app_recorded=$app_path; fi

# ---------------------------------------------------------------------- PATH

shell_name=$(basename "${SHELL:-sh}")
case $shell_name in
  zsh) rc_file=${ZDOTDIR:-$HOME}/.zshrc ;;
  bash)
    if [ "$os" = Darwin ]; then rc_file=$HOME/.bash_profile; else rc_file=$HOME/.bashrc; fi
    ;;
  fish) rc_file=${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/blirp.fish ;;
  *) rc_file=$HOME/.profile ;;
esac
if [ "$shell_name" = fish ]; then
  path_line="fish_add_path -g \"$install_dir\" $MARKER"
else
  path_line="export PATH=\"$install_dir:\$PATH\" $MARKER"
fi

on_path=
case ":${PATH:-}:" in *":$install_dir:"*) on_path=1 ;; esac
path_entry=
if [ -f "$rc_file" ] && grep -F "$MARKER" "$rc_file" >/dev/null 2>&1; then
  path_entry=$rc_file
fi
if [ -z "$on_path" ] && [ -z "$path_entry" ]; then
  if [ -n "$modify_path" ]; then
    mkdir -p "$(dirname "$rc_file")"
    printf '\n%s\n' "$path_line" >>"$rc_file"
    path_entry=$rc_file
    say "added $install_dir to PATH in $rc_file (open a new terminal to use it)"
  else
    say "$install_dir is not on your PATH. Add it with:"
    printf '\n    echo '\''%s'\'' >> %s\n\n' "$path_line" "$rc_file"
    say "or re-run this installer with --modify-path"
  fi
elif [ -z "$on_path" ]; then
  say "$install_dir is on PATH in new terminals ($path_entry)"
fi

# ------------------------------------------------------------------- receipt

json_str() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'; }
mkdir -p "$share_dir"
cat >"$receipt.tmp" <<EOF
{
  "version": "$(json_str "$ver")",
  "install_dir": "$(json_str "$install_dir")",
  "app": "$(json_str "$app_recorded")",
  "path_entry": "$(json_str "$path_entry")"
}
EOF
mv -f "$receipt.tmp" "$receipt"

# -------------------------------------------------------------------- daemon

if [ -n "$service" ]; then
  "$bin" service install
elif [ -n "$was_running" ]; then
  "$bin" daemon --detach >/dev/null && say "restarted the daemon"
fi

say "done: blirp $ver"
say "run \`blirp\` to open it, \`blirp --help\` for the CLI, \`blirp update\` to upgrade later"
