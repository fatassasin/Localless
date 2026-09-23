[中文](README.md) | **English**

# Localless: fully offline voice dictation

![Localless](docs/hero.webp)

Hold **Right Alt** and speak. When you let go, the text is typed into whatever input box has the cursor. No network, no account, no word limits.

The interface is currently in Chinese.

## Features
- **Local recognition**: Qwen3-ASR (1.7B or 0.6B) runs on your own GPU. Your audio never leaves your machine.
- **Custom words**: group them under tags (general, people, projects…) and recognition prefers those spellings.
- **History**: review, copy and compare every dictation against the raw transcript.
- **Clipboard left alone**: whatever you had copied is put back after each dictation. You can also keep dictations out of Win+V history.
- **Floating mic**: works over remote desktop and on touch screens.

## Requirements
- Windows 10 / 11 with an NVIDIA GPU. A CPU-only mode exists, but it is slow.
- [Python 3.11](https://www.python.org/downloads/)
- Rust, only if you build from source

## Install (release package)
1. Download the zip from [Releases](https://github.com/fatassasin/Localless/releases) and extract it anywhere.
2. In the extracted folder, run the command below. It builds the Python environment, installs PyTorch (CUDA build) and downloads the speech model, about 7 GB in total:
   ```bash
   powershell -ExecutionPolicy Bypass -File setup.ps1
   ```
   On a GPU with little VRAM, add `-Model qwen3-asr-0.6b-hf`, then pick it under Settings → Models. Without an NVIDIA GPU, add `-Cpu`.
3. Double-click `localless.exe`.

## Build from source
Run `setup.ps1` above first, then:
```bash
cd tauri/src-tauri && cargo build --release
```

## Launch
- **Double-click** `localless.exe` (a source build puts it in `tauri/src-tauri/target/release/`).
- **Start with Windows**: Settings → General → Launch at startup.

## Shortcut
| Action | Key |
|---|---|
| Dictate (hold to talk) | Right Alt |

This is the only shortcut, and you can change it in Settings. Localless registers no other global hotkeys.

## Your data
Settings, custom words, dictation history and recordings live only on your machine, in `%APPDATA%\localless\`. **None of it is in this repository**, and nothing is uploaded anywhere.
[`docs/localless-settings.example.json`](docs/localless-settings.example.json) is a generic settings template, provided only to show the format. The app writes its own defaults on first run.

## Layout
- `tauri/src-tauri/`: Rust shell. Handles windows, tray, keyboard hook, clipboard injection and the history database.
- `tauri/src/`: UI, including the pill, floating mic and settings page. It is compiled into the binary, so rerun `cargo build` after changes.
- `app/engine.py`: local recognition service, a WebSocket server on `127.0.0.1:8765`.
- `app/*.ps1`: three standalone sidecars for the keyboard hook, per-app muting and reading the focused control.
- `models/`: model weights, not tracked in git.

## Stop
Right-click the tray icon → Exit.

## Tests
```bash
cd tauri/src-tauri && cargo test
```
```bash
node tauri/tests/pages.test.js && node tauri/tests/recorder.test.js && node tauri/tests/prompts.test.js
```
```bash
app/.venv/Scripts/python.exe -m unittest discover -s app -p "test_*.py"
```

## License
[CC BY-NC 4.0](https://creativecommons.org/licenses/by-nc/4.0/): you may use, modify and share it freely, as long as you give credit and don't use it commercially. Full text in `LICENSE`.
Model weights are not part of this repository and follow their original authors' licenses.
