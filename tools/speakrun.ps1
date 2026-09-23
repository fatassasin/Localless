# Wrapper so the whole test runs inside ONE visible console window.
#
# Two reasons it is a file and not an inline -Command: the nested quoting of
# Start-Process ... -ArgumentList '-Command','...' silently produced a window
# that ran nothing (no output, no error), and the dictation result gets pasted
# into whatever window has focus -- so the test needs a window of its own to
# catch it instead of dropping the text into whatever the user was using.
#
# ASCII only: Windows PowerShell reads .ps1 as ANSI. The sentence to speak is
# read from a UTF-8 file at runtime, never written as a literal here.
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
Start-Transcript -Path "$env:TEMP\speakrun.log" -Force | Out-Null

Write-Host "pid=$PID"
# Let the window settle as foreground before anything is injected.
Start-Sleep -Seconds 3

$say = [IO.File]::ReadAllText("$env:TEMP\say.txt", [Text.Encoding]::UTF8)
Write-Host "chars=$($say.Length)"

& "$root\tools\speaktest.ps1" -Say $say

# Stay alive long enough to receive the paste, then go.
Start-Sleep -Seconds 25
Stop-Transcript | Out-Null
