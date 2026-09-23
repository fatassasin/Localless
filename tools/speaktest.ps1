# Drive one real dictation end to end: hold the trigger key, speak through the
# speakers with the system TTS voice, release, and let the app paste the result.
#
# Why a real keybd_event and a real voice instead of feeding the engine a wav:
# the only leg never proven on the Tauri build was "sound in the room -> text on
# screen". Every shortcut around it (posting to the ws, injecting a wav) skips
# exactly the part that was unproven.
#
# ASCII only on purpose: Windows PowerShell reads .ps1 as ANSI, so a non-ASCII
# literal here arrives at the API mangled. The sentence to speak is passed in.
#
# The caller is responsible for turning "mute other apps while recording" off --
# leave it on and this script mutes its own voice.
param(
  [string]$Say = '',
  [int]$LeadMs = 700,
  [int]$TailMs = 700
)
$ErrorActionPreference = 'Stop'

Add-Type @"
using System;using System.Runtime.InteropServices;
public class K {
  [DllImport("user32.dll")] public static extern void keybd_event(byte k, byte s, uint f, UIntPtr e);
}
"@

# VK_RMENU. KEYEVENTF_EXTENDEDKEY (0x01) is required: without it the low-level
# hook sees plain Alt, not RightAlt, and the trigger never matches.
$VK_RMENU = 0xA5
$EXT = 0x0001
$UP  = 0x0002

Add-Type -AssemblyName System.Speech
$tts = New-Object System.Speech.Synthesis.SpeechSynthesizer
$zh = $tts.GetInstalledVoices() | Where-Object { $_.VoiceInfo.Culture.Name -like 'zh*' } | Select-Object -First 1
if ($zh) { $tts.SelectVoice($zh.VoiceInfo.Name) }
$tts.Volume = 100
$tts.Rate = -1
Write-Host "voice=$($tts.Voice.Name)"

# Tap, not hold. The trigger is a toggle and it fires on the DOWN edge only:
# holding the key through the sentence gives it one edge, so recording starts
# and never stops. That cost a run -- the log showed `start rec-...` with no
# matching `done`, and the recorder was still going after the script exited.
function Tap {
  [K]::keybd_event($VK_RMENU, 0, $EXT, [UIntPtr]::Zero)
  Start-Sleep -Milliseconds 60
  [K]::keybd_event($VK_RMENU, 0, ($EXT -bor $UP), [UIntPtr]::Zero)
}

Tap
Write-Host 'tap 1 (start)'
Start-Sleep -Milliseconds $LeadMs
$tts.Speak($Say)
Start-Sleep -Milliseconds $TailMs
Tap
Write-Host 'tap 2 (stop)'
