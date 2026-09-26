#!/usr/bin/env sh
# Fill the packaging/ templates (Homebrew formula, winget manifests)
# with a release's version and asset checksums.
#
#   scripts/render-packaging.sh <version> <dir with the release assets> <out dir>
#
# The release workflow runs this after all assets are uploaded and attaches
# the output to the draft release; copy the files into the Homebrew tap and a
# winget-pkgs pull request when publishing.
set -eu

if [ $# -ne 3 ]; then
  echo "usage: $0 <version> <assets-dir> <out-dir>" >&2
  exit 2
fi
version=$1
assets=$2
out=$3
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)

sha() {
  f="$assets/$1"
  if [ ! -f "$f" ]; then
    echo "missing release asset: $1" >&2
    exit 1
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$f" | cut -d' ' -f1
  else
    shasum -a 256 "$f" | cut -d' ' -f1
  fi
}

v=$version
cli_mac_arm=$(sha "blirp-$v-aarch64-apple-darwin.tar.gz")
cli_mac_x64=$(sha "blirp-$v-x86_64-apple-darwin.tar.gz")
cli_linux_arm=$(sha "blirp-$v-aarch64-unknown-linux-gnu.tar.gz")
cli_linux_x64=$(sha "blirp-$v-x86_64-unknown-linux-gnu.tar.gz")
nsis_x64=$(sha "blirp_${v}_x64-setup.exe")

render() {
  sed -e "s/{{VERSION}}/$v/g" \
    -e "s/{{SHA256_CLI_MACOS_ARM64}}/$cli_mac_arm/g" \
    -e "s/{{SHA256_CLI_MACOS_X64}}/$cli_mac_x64/g" \
    -e "s/{{SHA256_CLI_LINUX_ARM64}}/$cli_linux_arm/g" \
    -e "s/{{SHA256_CLI_LINUX_X64}}/$cli_linux_x64/g" \
    -e "s/{{SHA256_NSIS_X64}}/$nsis_x64/g" \
    "$1" > "$2"
  if grep -n '{{' "$2" >&2; then
    echo "unfilled placeholder in $2" >&2
    exit 1
  fi
}

mkdir -p "$out"
render "$root/packaging/homebrew/blirp.rb" "$out/homebrew-formula-blirp.rb"
for f in "$root"/packaging/winget/*.yaml; do
  render "$f" "$out/winget-$(basename "$f")"
done
echo "rendered packaging manifests for $v into $out"
