#!/usr/bin/env sh
# Build the web UI and a release `blirp` binary, and stage it as the Tauri
# sidecar: app/src-tauri/binaries/blirp-<target>.
#
#   scripts/build-sidecar.sh                          # host target
#   scripts/build-sidecar.sh aarch64-apple-darwin     # explicit target
#   SKIP_WEB=1 scripts/build-sidecar.sh               # reuse web/dist
#
# Windows targets: use scripts/build-sidecar.ps1 (it also bundles ConPTY).
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
target=${1:-$(rustc -vV | sed -n 's/^host: //p')}
if [ -z "$target" ]; then
  echo "cannot determine the host target triple from rustc -vV" >&2
  exit 1
fi
case "$target" in
  *-windows-*)
    echo "use scripts/build-sidecar.ps1 for Windows targets" >&2
    exit 1
    ;;
esac

if [ "${SKIP_WEB:-0}" != "1" ]; then
  pnpm -C "$root/web" install --frozen-lockfile
  pnpm -C "$root/web" build
fi
if [ ! -f "$root/web/dist/index.html" ]; then
  echo "web/dist is missing; run without SKIP_WEB=1" >&2
  exit 1
fi

cargo build --release --locked -p blirp --target "$target" --manifest-path "$root/Cargo.toml"

bin="$root/app/src-tauri/binaries"
mkdir -p "$bin"
cp "$root/target/$target/release/blirp" "$bin/blirp-$target"
chmod +x "$bin/blirp-$target"
echo "sidecar ready: $bin/blirp-$target"
