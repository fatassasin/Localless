# Localless first-time setup: builds app\.venv and downloads the speech model.
#
#   powershell -ExecutionPolicy Bypass -File setup.ps1
#   powershell -ExecutionPolicy Bypass -File setup.ps1 -Model qwen3-asr-0.6b-hf
#
# Safe to rerun: an existing venv is reused, and download-model.py skips files
# that are already there and verified.
#
# ASCII only on purpose. Windows PowerShell 5.1 reads a .ps1 without a BOM as
# the ANSI code page, so any Chinese text here would turn into mojibake.
param(
    [ValidateSet('qwen3-asr-1.7b-hf', 'qwen3-asr-0.6b-hf')]
    [string]$Model = 'qwen3-asr-1.7b-hf',
    [switch]$Cpu
)
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$venv = Join-Path $root 'app\.venv'
$py = Join-Path $venv 'Scripts\python.exe'

# engine.py and every pinned dependency are tested on 3.11 only.
function Find-Python311 {
    foreach ($c in @(@('py', '-3.11'), @('python'), @('python3'))) {
        $exe = $c[0]; $pre = @($c | Select-Object -Skip 1)
        if (-not (Get-Command $exe -ErrorAction SilentlyContinue)) { continue }
        $v = & $exe @pre -c "import sys; print('%d.%d' % sys.version_info[:2])" 2>$null
        if ($LASTEXITCODE -eq 0 -and $v -eq '3.11') { return , $c }
    }
    return $null
}

if (-not (Test-Path $py)) {
    $base = Find-Python311
    if (-not $base) {
        Write-Host 'Python 3.11 not found. Install it from https://www.python.org/downloads/ and rerun.' -ForegroundColor Red
        exit 1
    }
    Write-Host "Creating app\.venv ..."
    $exe = $base[0]; $pre = @($base | Select-Object -Skip 1)
    & $exe @pre -m venv $venv
    if ($LASTEXITCODE -ne 0) { exit 1 }
}

& $py -m pip install --upgrade pip
if ($LASTEXITCODE -ne 0) { exit 1 }

# torch first, from the CUDA index: the PyPI wheel is CPU-only and would
# silently make every dictation several times slower.
$index = if ($Cpu) { 'https://download.pytorch.org/whl/cpu' } else { 'https://download.pytorch.org/whl/cu128' }
Write-Host "Installing PyTorch from $index (about 3 GB) ..."
& $py -m pip install torch==2.11.0 --index-url $index
if ($LASTEXITCODE -ne 0) { exit 1 }

& $py -m pip install -r (Join-Path $root 'app\requirements-qwen-asr.txt')
if ($LASTEXITCODE -ne 0) { exit 1 }

$models = Join-Path $root 'models'
New-Item -ItemType Directory -Force -Path $models | Out-Null
Write-Host "Downloading $Model ..."
& $py (Join-Path $root 'app\download-model.py') --model $Model --models-dir $models
if ($LASTEXITCODE -ne 0) { exit 1 }

Write-Host 'Done. Start localless.exe.' -ForegroundColor Green
