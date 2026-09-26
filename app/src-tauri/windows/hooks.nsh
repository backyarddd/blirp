; NSIS installer hooks (bundle.windows.nsis.installerHooks).
;
; A running blirp daemon keeps $INSTDIR\blirp.exe locked, so installing over
; it (manual upgrade or repair) or uninstalling would fail with "error opening
; file for writing". Before touching files, stop the daemon if it runs from
; this install directory: first gracefully over its API (sessions end as
; "detached", like Quit in the tray), then force whatever is left. The in-app
; updater already stops the daemon, so there this is a no-op.
;
; The PowerShell below is NSIS-escaped: $$ is a literal $; the NSIS string is
; delimited by backticks, the -Command argument by double quotes, so the
; script itself only uses single quotes.

!macro BLIRP_STOP_DAEMON
  System::Call 'Kernel32::SetEnvironmentVariable(t "BLIRP_INSTDIR", t "$INSTDIR")i'
  Push $0
  nsExec::Exec `powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$$d = $$env:BLIRP_INSTDIR; $$h = if ($$env:BLIRP_HOME) { $$env:BLIRP_HOME } else { Join-Path $$env:USERPROFILE '.blirp' }; try { $$r = Get-Content (Join-Path $$h 'runtime.json') -Raw -ErrorAction Stop | ConvertFrom-Json; $$p = Get-Process -Id $$r.pid -ErrorAction Stop; if ($$p.Path -like ($$d + '\*')) { Invoke-WebRequest -UseBasicParsing -Method Post -Uri ('http://127.0.0.1:' + $$r.port + '/api/daemon/shutdown') -Headers @{ Authorization = ('Bearer ' + $$r.token) } -TimeoutSec 5 | Out-Null; $$p.WaitForExit(15000) | Out-Null } } catch { }; Get-Process -Name blirp -ErrorAction SilentlyContinue | Where-Object { $$_.Path -like ($$d + '\*') } | Stop-Process -Force"`
  Pop $0 ; exit code, ignored: failing to stop must not block the install
  Pop $0
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro BLIRP_STOP_DAEMON
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro BLIRP_STOP_DAEMON
!macroend
