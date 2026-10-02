# Verify the logon autostart entry actually starts the app.
#
# Autostart is a Localless.lnk in the per-user Startup folder (Run-key entries were
# never executed at logon on this machine and showed up iconless in Settings).
# The shortcut target is read back from the .lnk rather than retyped: the repo
# path contains non-ASCII characters, so typing it here would both mangle
# (Windows PowerShell reads .ps1 as ANSI) and test the wrong string.
#
# ASCII only on purpose, same reason.
$ErrorActionPreference = 'Stop'

$lnk = Join-Path ([Environment]::GetFolderPath('Startup')) 'Localless.lnk'
$target = (New-Object -ComObject WScript.Shell).CreateShortcut($lnk).TargetPath
Write-Host "shortcut: $lnk -> $target"

$old = Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name Localless -ErrorAction SilentlyContinue
if ($old) { Write-Host "WARNING: Run\Localless still present, logon would start two copies" }

$before = @(Get-Process localless -ErrorAction SilentlyContinue).Count
Write-Host "before: localless.exe=$before"

# Launch through the shell like Explorer does for Startup-folder items.
Start-Process -FilePath $lnk
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
