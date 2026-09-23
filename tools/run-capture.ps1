# Launch the Tauri build with stderr captured to a file.
#
# The normal launcher goes through Localless.vbs with the console hidden, which
# throws stderr away -- and every pill/settings diagnostic in this app is an
# eprintln. Without this, "the pill never appeared" has no evidence at all.
#
# -Settings sets LOCALLESS_SETTINGS so the settings window opens at startup
# instead of having to be clicked out of the tray.
#
# It is a file and not an inline -Command on purpose: the nested quoting of
# Start-Process ... -RedirectStandardError through bash mangles every time.
#
# ASCII only: Windows PowerShell reads .ps1 as ANSI.
param([switch]$Settings)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$exe  = Join-Path $root 'tauri\src-tauri\target\release\localless.exe'
$err  = Join-Path $env:TEMP 'localless-stderr.log'
$out  = Join-Path $env:TEMP 'localless-stdout.log'
Remove-Item $err, $out -ErrorAction SilentlyContinue
if ($Settings) { $env:LOCALLESS_SETTINGS = '1' } else { Remove-Item Env:\LOCALLESS_SETTINGS -ErrorAction SilentlyContinue }
$p = Start-Process -FilePath $exe -PassThru -RedirectStandardError $err -RedirectStandardOutput $out
Write-Host "pid=$($p.Id) settings=$($Settings.IsPresent) stderr=$err"
