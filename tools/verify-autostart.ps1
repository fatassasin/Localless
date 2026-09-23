# Verify the logon autostart entry actually starts the app.
#
# It runs the HKCU Run value VERBATIM, read back from the registry, rather than a
# hand-retyped equivalent. The repo path contains non-ASCII characters, so typing
# it here would both mangle (Windows PowerShell reads .ps1 as ANSI) and test the
# wrong string -- the point is to prove what Windows will actually execute.
#
# ASCII only on purpose, same reason.
$ErrorActionPreference = 'Stop'

$key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$cmd = (Get-ItemProperty $key -Name Localless).Localless
Write-Host "RUN value: $cmd"

$before = @(Get-Process localless -ErrorAction SilentlyContinue).Count
Write-Host "before: localless.exe=$before"

# Explorer launches Run entries through CreateProcess with the value as the
# command line. 'cmd /c' with the value unquoted reproduces that closely enough:
# wscript.exe resolves off PATH and the quoted script path stays one argument.
cmd /c $cmd
Start-Sleep -Seconds 14

Write-Host '--- after ---'
Get-CimInstance Win32_Process -Filter "Name='localless.exe'" |
  Select-Object ProcessId, CommandLine | Format-List

$hooks = @(Get-CimInstance Win32_Process -Filter "Name='powershell.exe'" |
  Where-Object { $_.CommandLine -like '*-File*keyhook-localless.ps1*' })
Write-Host "hooks=$($hooks.Count) pids=$($hooks.ProcessId -join ',')"

$eng = @(Get-CimInstance Win32_Process -Filter "Name='python.exe'" |
  Where-Object { $_.CommandLine -like '*Localless\app\engine.py*' })
Write-Host "engine=$($eng.Count) pids=$($eng.ProcessId -join ',')"

$qs = @(Get-CimInstance Win32_Process -Filter "Name='electron.exe'" |
  Where-Object { $_.CommandLine -like '*QuotaSidebar*' })
Write-Host "QuotaSidebar electron untouched: $($qs.Count)"

# A leftover console would mean the hidden-window launcher hung on something.
$stray = @(Get-CimInstance Win32_Process -Filter "Name='cmd.exe'" |
  Where-Object { $_.CommandLine -like '*Localless*' })
Write-Host "stray cmd.exe holding the launcher: $($stray.Count)"
