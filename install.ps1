<#
.SYNOPSIS
  blirp installer for Windows (x64; Windows PowerShell 5.1 or PowerShell 7).

.DESCRIPTION
  Installs blirp.exe (with conpty.dll and x64\OpenConsole.exe) and the desktop
  app into $env:BLIRP_INSTALL_DIR (default %LOCALAPPDATA%\Programs\blirp),
  adds that folder to your user Path, and creates a Start Menu shortcut.
  Every download is checked against the release's SHA256SUMS.txt, and that
  against its minisign signature when minisign or an OpenSSL 3 (Git for
  Windows ships one) is available. Running it again upgrades in place. No
  administrator rights are needed.

  Options can also be set with environment variables, which is the only way
  when piping into iex: BLIRP_VERSION, BLIRP_NO_APP=1, BLIRP_SERVICE=1,
  BLIRP_NO_MODIFY_PATH=1, BLIRP_INSTALL_DIR, GITHUB_TOKEN (private repository
  or rate limits), BLIRP_RELEASE_BASE_URL (releases API of a mirror or test
  server instead of GitHub), BLIRP_REQUIRE_SIGNATURE=1 (refuse to install
  when the signature cannot be checked).

.EXAMPLE
  irm https://raw.githubusercontent.com/backyarddd/blirp/main/install.ps1 | iex

.EXAMPLE
  & ([scriptblock]::Create((irm https://raw.githubusercontent.com/backyarddd/blirp/main/install.ps1))) -Service
#>
# Parameters are read in the child scope below; Write-Host is the installer's
# progress output (return values of the helpers must stay clean).
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSReviewUnusedParameter', '')]
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '')]
[CmdletBinding()]
param(
  # Install this release instead of the latest.
  [string]$Version = $env:BLIRP_VERSION,
  # CLI only, no desktop app.
  [switch]$NoApp = ($env:BLIRP_NO_APP -eq '1'),
  # Start the daemon at login (`blirp service install`).
  [switch]$Service = ($env:BLIRP_SERVICE -eq '1'),
  # Accepted for parity with install.sh; adding to the user Path is the default here.
  [switch]$ModifyPath,
  # Leave the user Path alone.
  [switch]$NoModifyPath = ($env:BLIRP_NO_MODIFY_PATH -eq '1')
)

# A child scope: with `irm | iex` this runs in the caller's session, so keep
# preferences and helpers out of it. Errors are thrown, never `exit`, which
# would close that session.
& {
  $ErrorActionPreference = 'Stop'
  # Windows PowerShell's progress bar slows downloads down a lot.
  $ProgressPreference = 'SilentlyContinue'
  # Windows PowerShell 5.1 may default to TLS 1.0; GitHub needs 1.2.
  [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

  $ReleasesApi = 'https://api.github.com/repos/backyarddd/blirp/releases'
  # The release signing key, packaging/minisign.pub.
  $ReleasePubkey = 'RWQUGAux2LF3nsIhgzZsZL6OfhV6O3mpN1jUyApoyh04lnSVLsoZ2NX5'
  $Triple = 'x86_64-pc-windows-msvc'
  $CliFiles = @('blirp.exe', 'conpty.dll', 'x64\OpenConsole.exe')
  $DesktopExe = 'blirp-desktop.exe'

  function Say([string]$msg) { Write-Host "blirp: $msg" }

  if (-not [Environment]::Is64BitOperatingSystem) { throw 'blirp needs 64-bit Windows.' }
  if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { Say 'ARM64 Windows: installing the x64 build (runs under emulation)' }

  $installDir = $env:BLIRP_INSTALL_DIR
  if (-not $installDir) { $installDir = Join-Path $env:LOCALAPPDATA 'Programs\blirp' }
  $installDir = [IO.Path]::GetFullPath($installDir).TrimEnd('\')
  $token = $env:GITHUB_TOKEN
  $base = $env:BLIRP_RELEASE_BASE_URL
  if (-not $base) { $base = $ReleasesApi }
  $base = $base.TrimEnd('/')

  function Get-Url([string]$url, [string]$outFile, [string]$accept) {
    $headers = @{}
    if ($accept) { $headers['Accept'] = $accept }
    # Both PowerShell editions drop Authorization on the redirect to the download host.
    if ($token) { $headers['Authorization'] = "Bearer $token" }
    Invoke-WebRequest -Uri $url -OutFile $outFile -Headers $headers -UseBasicParsing
  }

  $tmp = Join-Path ([IO.Path]::GetTempPath()) ('blirp-install-' + [guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory -Path $tmp | Out-Null
  try {
    # ------------------------------------------------------------ release
    $v = if ($Version) { $Version.Trim().TrimStart('v') } else { '' }
    $releaseUrl = if ($v) { "$base/tags/v$v" } else { "$base/latest" }
    $releaseFile = Join-Path $tmp 'release.json'
    try {
      Get-Url $releaseUrl $releaseFile 'application/vnd.github+json'
    } catch {
      $what = if ($v) { "release v$v not found" } else { 'no published release found' }
      $hint = if ($token) { '' } else { ' (private repository? set $env:GITHUB_TOKEN)' }
      throw "$what at $base$hint`: $($_.Exception.Message)"
    }
    $release = [IO.File]::ReadAllText($releaseFile) | ConvertFrom-Json
    $ver = ([string]$release.tag_name).TrimStart('v')
    if (-not $ver) { throw "unexpected release data from $releaseUrl" }

    function Test-Asset([string]$name) { [bool]($release.assets | Where-Object { $_.name -eq $name }) }
    function Save-Asset([string]$name) {
      $asset = $release.assets | Where-Object { $_.name -eq $name } | Select-Object -First 1
      if (-not $asset) { throw "release $($release.tag_name) has no asset $name" }
      $out = Join-Path $tmp $name
      if ($token) { Get-Url $asset.url $out 'application/octet-stream' } else { Get-Url $asset.browser_download_url $out '' }
      $out
    }
    $sumsFile = Save-Asset 'SHA256SUMS.txt'
    $sigFile = Save-Asset 'SHA256SUMS.txt.sig'

    # ------------------------------------------------------------ signature
    # The checksums are only as trustworthy as their signature. .NET has
    # neither Ed25519 nor BLAKE2b, so check it with minisign or an OpenSSL 3
    # (on PATH, or the one Git for Windows ships); without either, the
    # checksums came over HTTPS from GitHub, like this script.
    function Invoke-Quiet([string]$exe, [string[]]$argv) {
      # Native stderr must not become a terminating error here.
      $ErrorActionPreference = 'Continue'
      & $exe @argv 2>$null | Out-Null
      $LASTEXITCODE -eq 0
    }
    function Find-OpenSsl {
      $candidates = @()
      $onPath = Get-Command openssl.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
      if ($onPath) { $candidates += $onPath.Source }
      $git = Get-Command git.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
      if ($git) {
        $gitRoot = Split-Path -Parent (Split-Path -Parent $git.Source)
        $candidates += (Join-Path $gitRoot 'usr\bin\openssl.exe'), (Join-Path $gitRoot 'mingw64\bin\openssl.exe')
      }
      $probe = Join-Path $tmp 'probe.txt'
      [IO.File]::WriteAllText($probe, '')
      foreach ($c in $candidates) {
        if (-not (Test-Path -LiteralPath $c -PathType Leaf)) { continue }
        if (-not (Invoke-Quiet $c @('dgst', '-blake2b512', '-binary', '-out', (Join-Path $tmp 'probe.out'), $probe))) { continue }
        $ErrorActionPreference = 'Continue'
        $help = (& $c pkeyutl -help 2>&1 | Out-String)
        if ($help -match '-rawin') { return $c }
      }
      $null
    }
    # Check the minisign signature $sig of $file with $ossl against $key:
    # the key is base64 of "Ed" + key id (8) + Ed25519 key (32); the
    # signature line base64 of the algorithm ("ED": over BLAKE2b-512 of the
    # file, "Ed": over the file) + key id (8) + signature (64); the global
    # signature covers that signature and the trusted comment.
    function Test-MinisignWithOpenSsl([string]$ossl, [string]$file, [string]$sig, [string]$key) {
      $prefix = 'trusted comment: '
      try {
        $pub = [Convert]::FromBase64String($key)
        $lines = [IO.File]::ReadAllText($sig) -split "`r?`n"
        if ($lines.Count -lt 4 -or -not $lines[2].StartsWith($prefix)) { return $false }
        $s = [Convert]::FromBase64String($lines[1].Trim())
        $global = [Convert]::FromBase64String($lines[3].Trim())
      } catch {
        return $false
      }
      if ($pub.Length -ne 42 -or $s.Length -ne 74 -or $global.Length -ne 64) { return $false }
      for ($i = 2; $i -lt 10; $i++) { if ($pub[$i] -ne $s[$i]) { return $false } }
      $d = Join-Path $tmp 'sigcheck'
      New-Item -ItemType Directory -Force -Path $d | Out-Null
      # Ed25519 SubjectPublicKeyInfo: fixed DER prefix + the 32 key bytes.
      $der = [byte[]](0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00) + $pub[10..41]
      $keyFile = Join-Path $d 'key.der'
      [IO.File]::WriteAllBytes($keyFile, [byte[]]$der)
      $sigRaw = [byte[]]$s[10..73]
      $sigPath = Join-Path $d 'sig.raw'
      [IO.File]::WriteAllBytes($sigPath, $sigRaw)
      $msg = Join-Path $d 'msg'
      $alg = [Text.Encoding]::ASCII.GetString($s, 0, 2)
      if ($alg -ceq 'ED') {
        if (-not (Invoke-Quiet $ossl @('dgst', '-blake2b512', '-binary', '-out', $msg, $file))) { return $false }
      } elseif ($alg -ceq 'Ed') {
        Copy-Item -LiteralPath $file -Destination $msg -Force
      } else {
        return $false
      }
      $verify = @('pkeyutl', '-verify', '-pubin', '-keyform', 'DER', '-inkey', $keyFile, '-rawin')
      if (-not (Invoke-Quiet $ossl ($verify + @('-in', $msg, '-sigfile', $sigPath)))) { return $false }
      $comment = [Text.Encoding]::UTF8.GetBytes($lines[2].Substring($prefix.Length))
      $globalMsg = Join-Path $d 'global.msg'
      [IO.File]::WriteAllBytes($globalMsg, [byte[]]($sigRaw + $comment))
      $globalSig = Join-Path $d 'global.sig'
      [IO.File]::WriteAllBytes($globalSig, $global)
      Invoke-Quiet $ossl ($verify + @('-in', $globalMsg, '-sigfile', $globalSig))
    }
    $notSigned = "SHA256SUMS.txt of $($release.tag_name) is not signed by the blirp release key"
    $minisign = Get-Command minisign.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    $ossl = if ($minisign) { $null } else { Find-OpenSsl }
    if ($minisign) {
      if (-not (Invoke-Quiet $minisign.Source @('-V', '-q', '-m', $sumsFile, '-x', $sigFile, '-P', $ReleasePubkey))) { throw "$notSigned (checked with minisign); not installing" }
      Say 'release signature verified (minisign)'
    } elseif ($ossl) {
      if (-not (Test-MinisignWithOpenSsl -ossl $ossl -file $sumsFile -sig $sigFile -key $ReleasePubkey)) { throw "$notSigned (checked with $ossl); not installing" }
      Say "release signature verified ($ossl)"
    } elseif ($env:BLIRP_REQUIRE_SIGNATURE -and $env:BLIRP_REQUIRE_SIGNATURE -notin @('0', 'false')) {
      throw 'BLIRP_REQUIRE_SIGNATURE is set but nothing here can check a minisign signature (install minisign, or Git for Windows / OpenSSL 3)'
    } else {
      Say 'notice: the release signature was not checked (that needs minisign, or an OpenSSL 3 such as the one in Git for Windows).'
      Say 'notice: the downloads are checked against SHA256SUMS.txt fetched over HTTPS from GitHub, like this script.'
      Say 'notice: `blirp update` checks the signature itself. $env:BLIRP_REQUIRE_SIGNATURE = ''1'' refuses to install without it.'
    }

    $sums = @{}
    foreach ($line in [IO.File]::ReadAllLines($sumsFile)) {
      if ($line -match '^([0-9a-fA-F]{64})\s+\*?(.+?)\s*$') { $sums[$Matches[2]] = $Matches[1].ToLowerInvariant() }
    }
    # Download an asset, check it against SHA256SUMS.txt, return its path.
    function Get-Verified([string]$name) {
      $file = Save-Asset $name
      if (-not $sums.ContainsKey($name)) { throw "$name is not listed in SHA256SUMS.txt" }
      $got = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash.ToLowerInvariant()
      if ($got -ne $sums[$name]) { throw "checksum mismatch for $name (expected $($sums[$name]), got $got)" }
      $file
    }

    Say "installing blirp $ver ($Triple)"
    $cliName = "blirp-$ver-$Triple"
    if (-not (Test-Asset "$cliName.zip")) { throw "release $($release.tag_name) has no build for $Triple" }
    $cliZip = Get-Verified "$cliName.zip"
    Expand-Archive -LiteralPath $cliZip -DestinationPath (Join-Path $tmp 'cli') -Force
    $cliRoot = Join-Path $tmp "cli\$cliName"
    $sources = [ordered]@{}
    foreach ($f in $CliFiles) {
      $src = Join-Path $cliRoot $f
      if (-not (Test-Path -LiteralPath $src -PathType Leaf)) { throw "$cliName.zip does not contain $cliName\$f" }
      $sources[$f] = $src
    }
    $appName = "blirp_${ver}_x64-portable"
    if (-not $NoApp) {
      if (Test-Asset "$appName.zip") {
        $appZip = Get-Verified "$appName.zip"
        Expand-Archive -LiteralPath $appZip -DestinationPath (Join-Path $tmp 'app') -Force
        $src = Join-Path $tmp "app\$appName\$DesktopExe"
        if (-not (Test-Path -LiteralPath $src -PathType Leaf)) { throw "$appName.zip does not contain $appName\$DesktopExe" }
        $sources[$DesktopExe] = $src
      } else {
        Say "warning: release $($release.tag_name) has no desktop app ($appName.zip); installing the CLI only"
      }
    }

    # ------------------------------------------------------------ install
    $exe = Join-Path $installDir 'blirp.exe'
    $wasRunning = $false
    if (Test-Path -LiteralPath $exe) {
      & $exe status *> $null
      if ($LASTEXITCODE -eq 0) {
        $wasRunning = $true
        Say 'stopping the running daemon for the upgrade'
        & $exe stop | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'could not stop the running daemon (blirp stop)' }
      }
    }

    # A running exe cannot be overwritten or deleted but can be renamed: move
    # the old file aside, then the new one in. blirp removes leftover *.old
    # files the next time it starts.
    function Install-File([string]$src, [string]$dst) {
      $dir = Split-Path -Parent $dst
      New-Item -ItemType Directory -Force -Path $dir | Out-Null
      $staged = "$dst.new"
      Copy-Item -LiteralPath $src -Destination $staged -Force
      if (Test-Path -LiteralPath $dst) {
        $aside = "$dst.old"
        if (Test-Path -LiteralPath $aside) { Remove-Item -LiteralPath $aside -Force -ErrorAction SilentlyContinue }
        if (Test-Path -LiteralPath $aside) { $aside = "$dst.$([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()).old" }
        Move-Item -LiteralPath $dst -Destination $aside
        try {
          Move-Item -LiteralPath $staged -Destination $dst
        } catch {
          Move-Item -LiteralPath $aside -Destination $dst
          throw
        }
        Remove-Item -LiteralPath $aside -Force -ErrorAction SilentlyContinue
      } else {
        Move-Item -LiteralPath $staged -Destination $dst
      }
      Unblock-File -LiteralPath $dst -ErrorAction SilentlyContinue
    }
    foreach ($f in $sources.Keys) { Install-File $sources[$f] (Join-Path $installDir $f) }
    Say "installed $exe"

    $appPath = Join-Path $installDir $DesktopExe
    $shortcut = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\blirp.lnk'
    if (Test-Path -LiteralPath $appPath) {
      $shell = New-Object -ComObject WScript.Shell
      $lnk = $shell.CreateShortcut($shortcut)
      $lnk.TargetPath = $appPath
      $lnk.WorkingDirectory = $installDir
      $lnk.Description = 'Workspace and memory for CLI coding agents'
      $lnk.Save()
      if ($sources.Contains($DesktopExe)) { Say "installed the desktop app: $appPath (Start Menu: blirp)" }
    } else {
      $appPath = ''
    }

    # --------------------------------------------------------------- PATH
    $receiptFile = Join-Path $installDir 'install.json'
    $previousEntry = ''
    if (Test-Path -LiteralPath $receiptFile) {
      try { $previousEntry = [string](([IO.File]::ReadAllText($receiptFile) | ConvertFrom-Json).path_entry) } catch { $previousEntry = '' }
    }
    $envKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
    $userPath = [string]$envKey.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
    $onPath = @($userPath -split ';' | Where-Object { $_ -and ($_.TrimEnd('\') -eq $installDir) }).Count -gt 0
    $pathEntry = if ($onPath) { $previousEntry } else { '' }
    if (-not $onPath -and -not $NoModifyPath) {
      $newPath = if ($userPath) { "$($userPath.TrimEnd(';'));$installDir" } else { $installDir }
      $envKey.SetValue('Path', $newPath, 'ExpandString')
      # Setting any user variable through .NET broadcasts WM_SETTINGCHANGE,
      # so Explorer and new terminals pick up the new Path.
      [Environment]::SetEnvironmentVariable('BLIRP_PATH_CHANGED', '1', 'User')
      [Environment]::SetEnvironmentVariable('BLIRP_PATH_CHANGED', $null, 'User')
      $pathEntry = $installDir
      Say "added $installDir to your user Path (new terminals see it)"
    } elseif (-not $onPath) {
      Say "$installDir is not on your Path; run it as `"$exe`" or add the folder yourself"
    }
    $envKey.Close()
    if (@($env:Path -split ';' | Where-Object { $_.TrimEnd('\') -eq $installDir }).Count -eq 0 -and $pathEntry) {
      $env:Path = "$installDir;$env:Path"
    }

    # ------------------------------------------------------------ receipt
    $receipt = [ordered]@{
      version     = $ver
      install_dir = $installDir
      app         = $appPath
      path_entry  = $pathEntry
    }
    # UTF-8 without a byte order mark (Set-Content -Encoding UTF8 adds one on 5.1).
    [IO.File]::WriteAllText($receiptFile, ($receipt | ConvertTo-Json) + "`n")

    # ------------------------------------------------------------- daemon
    if ($Service) {
      & $exe service install
      if ($LASTEXITCODE -ne 0) { throw 'blirp service install failed' }
    }
    # Same as install.sh: `blirp start` knows the autostart service (a no-op
    # when the daemon already runs).
    if ($wasRunning) {
      & $exe start | Out-Null
      if ($LASTEXITCODE -eq 0) { Say 'restarted the daemon' } else { Say 'warning: the daemon did not start again; run `blirp start` and see `blirp logs`' }
    }

    Say "done: blirp $ver"
    Say 'run `blirp` to open it, `blirp --help` for the CLI, `blirp update` to upgrade later'
  } finally {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
  }
}
