#Requires -Version 7
<#
.SYNOPSIS
  Build the web UI and a release `blirp` binary, and stage it as the Tauri
  sidecar (app/src-tauri/binaries/blirp-<target>.exe) together with the
  bundled ConPTY (conpty.dll + x64/OpenConsole.exe).

.EXAMPLE
  scripts/build-sidecar.ps1
  scripts/build-sidecar.ps1 -Target x86_64-pc-windows-msvc -SkipWeb
#>
[CmdletBinding()]
param(
  # Rust target triple; defaults to the host.
  [string]$Target,
  # Reuse an existing web/dist.
  [switch]$SkipWeb
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

# Pinned so bundles are reproducible; bump both together, and refresh
# packaging/licenses/microsoft-terminal-* (THIRD_PARTY_NOTICES) from the
# matching microsoft/terminal release.
$ConPtyVersion = '1.24.260710001'
$ConPtySha256 = '175640566A3B59C4B132070EE96C2C77E5AB7EDD2E92732A5EB3610BBF63D90E'

function Invoke-Checked([string]$exe, [string[]]$argv) {
  & $exe @argv
  if ($LASTEXITCODE -ne 0) { throw "$exe $($argv -join ' ') failed with exit code $LASTEXITCODE" }
}

if (-not $Target) {
  $hostLine = (& rustc -vV) | Where-Object { $_ -like 'host: *' }
  if (-not $hostLine) { throw 'cannot determine the host target triple from rustc -vV' }
  $Target = $hostLine.Substring(6).Trim()
}
if ($Target -notlike '*-windows-*') {
  throw "build-sidecar.ps1 builds Windows targets; use scripts/build-sidecar.sh for $Target"
}
if ($Target -notlike 'x86_64-*') {
  throw 'only x86_64 Windows is supported (the ConPTY mapping in tauri.windows.conf.json is x64)'
}

if (-not $SkipWeb) {
  Invoke-Checked pnpm @('-C', "$root/web", 'install', '--frozen-lockfile')
  Invoke-Checked pnpm @('-C', "$root/web", 'build')
}
if (-not (Test-Path "$root/web/dist/index.html")) {
  throw 'web/dist is missing; run without -SkipWeb'
}

Invoke-Checked cargo @('build', '--release', '--locked', '-p', 'blirp', '--target', $Target, '--manifest-path', "$root/Cargo.toml")

$bin = Join-Path $root 'app/src-tauri/binaries'
New-Item -ItemType Directory -Force $bin | Out-Null
Copy-Item -Force "$root/target/$Target/release/blirp.exe" (Join-Path $bin "blirp-$Target.exe")

# ConPTY from the Microsoft.Windows.Console.ConPTY NuGet package. portable-pty
# loads conpty.dll by name, which Windows resolves from the executable's
# directory first; conpty.dll starts <arch>\OpenConsole.exe next to itself.
$cache = Join-Path $root "target/conpty-$ConPtyVersion"
$pkg = Join-Path $cache 'conpty.nupkg'
if (-not (Test-Path $pkg)) {
  New-Item -ItemType Directory -Force $cache | Out-Null
  $url = "https://api.nuget.org/v3-flatcontainer/microsoft.windows.console.conpty/$ConPtyVersion/microsoft.windows.console.conpty.$ConPtyVersion.nupkg"
  Invoke-WebRequest -Uri $url -OutFile "$pkg.part" -UseBasicParsing
  Move-Item -Force "$pkg.part" $pkg
}
$actual = (Get-FileHash $pkg -Algorithm SHA256).Hash
if ($actual -ne $ConPtySha256) {
  [System.IO.File]::Delete($pkg)
  throw "ConPTY package checksum mismatch: expected $ConPtySha256, got $actual"
}
$extract = Join-Path $cache 'pkg'
if (-not (Test-Path "$extract/runtimes")) {
  Add-Type -AssemblyName System.IO.Compression.FileSystem
  [System.IO.Compression.ZipFile]::ExtractToDirectory($pkg, $extract, $true)
}
$conpty = Join-Path $bin 'conpty'
New-Item -ItemType Directory -Force (Join-Path $conpty 'x64') | Out-Null
Copy-Item -Force "$extract/runtimes/win-x64/native/conpty.dll" (Join-Path $conpty 'conpty.dll')
Copy-Item -Force "$extract/build/native/runtimes/x64/OpenConsole.exe" (Join-Path $conpty 'x64/OpenConsole.exe')

Write-Host "sidecar ready: $(Join-Path $bin "blirp-$Target.exe")"
Write-Host "conpty ready:  $conpty"
