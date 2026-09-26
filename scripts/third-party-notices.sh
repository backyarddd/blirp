#!/usr/bin/env sh
# Write THIRD_PARTY_NOTICES: the licenses of everything blirp ships that it
# did not write. The release workflow runs this once and puts the file into
# every archive and installer.
#
#   scripts/third-party-notices.sh <out file>
#
# Needs cargo-about (the version release.yml pins; config: about.toml and
# packaging/about.hbs), node, pnpm, and web/ and app/ installed
# (`pnpm install --frozen-lockfile`).
set -eu

if [ $# -ne 1 ]; then
  echo "usage: $0 <out file>" >&2
  exit 2
fi
out=$1
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

cargo about generate --locked --manifest-path "$root/Cargo.toml" -c "$root/about.toml" \
  -o "$tmp/rust.txt" "$root/packaging/about.hbs"
for pkg in web app; do
  pnpm -C "$root/$pkg" licenses list --prod --json >"$tmp/$pkg.json"
done
# Production npm packages (web/ is bundled into the UI that blirp serves):
# each with the license and notice files it ships.
node - "$tmp/web.json" "$tmp/app.json" >"$tmp/npm.txt" <<'EOF'
const fs = require('node:fs');
const path = require('node:path');
const seen = new Set();
const entries = [];
for (const file of process.argv.slice(2)) {
  for (const [license, pkgs] of Object.entries(JSON.parse(fs.readFileSync(file, 'utf8')))) {
    for (const p of pkgs) {
      p.versions.forEach((version, i) => {
        const id = `${p.name}@${version}`;
        if (seen.has(id)) return;
        seen.add(id);
        entries.push({ name: p.name, version, license: license.replace(/^\((.*)\)$/, '$1'), homepage: p.homepage, dir: p.paths[i] });
      });
    }
  }
}
entries.sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));
let missing = 0;
for (const e of entries) {
  const files = fs.readdirSync(e.dir).filter((f) => /^(licen[cs]e|copying|notice)([.-].*)?$/i.test(f)).sort();
  const out = ['-'.repeat(80), `${e.name} ${e.version} (${e.license})${e.homepage ? ` ${e.homepage}` : ''}`, ''];
  if (files.length === 0) {
    missing++;
    console.error(`third-party-notices: ${e.name}@${e.version} ships no license file`);
    out.push(`The package ships no license file; its declared license is ${e.license}.`, '');
  }
  for (const f of files) out.push(fs.readFileSync(path.join(e.dir, f), 'utf8').replace(/\r\n/g, '\n').trimEnd(), '');
  process.stdout.write(out.join('\n') + '\n');
}
if (entries.length === 0) throw new Error('no npm packages found; is web/ installed?');
console.error(`third-party-notices: ${entries.length} npm packages (${missing} without a license file)`);
EOF

{
  cat <<'EOF'
blirp third-party notices

blirp is licensed under the Apache License 2.0 (see LICENSE). It includes
software written by others, under the licenses below:

  1. Rust crates compiled into blirp and the desktop app
  2. JavaScript packages bundled into the web UI
  3. ConPTY (conpty.dll, x64/OpenConsole.exe), shipped with the Windows builds

================================================================================
1. Rust crates
================================================================================
EOF
  cat "$tmp/rust.txt"
  cat <<'EOF'

================================================================================
2. JavaScript packages
================================================================================

EOF
  cat "$tmp/npm.txt"
  cat <<'EOF'

================================================================================
3. ConPTY
================================================================================

Microsoft.Windows.Console.ConPTY, from the Windows Terminal project
(https://github.com/microsoft/terminal), MIT License:

EOF
  cat "$root/packaging/licenses/microsoft-terminal-LICENSE"
  printf '\nNotices of the Windows Terminal project (NOTICE.md), whose code ConPTY shares:\n\n'
  cat "$root/packaging/licenses/microsoft-terminal-NOTICE.md"
} >"$tmp/notices"
mv -f "$tmp/notices" "$out"
echo "wrote $out"
