**中文** | [English](README.en.md)

# Localless：完全离线的语音听写

![Localless](docs/hero.webp)

按住 **右 Alt** 说话，松开后文字自动送进光标所在的输入框。不联网、不登录、没有字数限制。

## 功能
- **本地识别**：Qwen3-ASR（1.7B / 0.6B）跑在自己的 GPU 上，音频不出本机
- **自定义词语**：按标签分组（通用 / 人名 / 编程项目…），识别时优先选这些写法
- **历史**：每条听写可回看、复制、对照原文
- **不打扰剪贴板**：听写完原来复制的东西原样还原；可选不进 Win+V 历史
- **悬浮麦克风**：远程桌面、触屏下也能用

## 环境
- Windows 10 / 11，NVIDIA 显卡（也有纯 CPU 模式，但慢）
- Rust（编译 Tauri 壳）
- Python 3.11，装 PyTorch（CUDA 版）、`transformers`、`accelerate`、`websockets`、`numpy`、`scipy`、`soundfile`

## 安装
```bash
python -m venv app/.venv
```
```bash
app/.venv/Scripts/python.exe app/download-model.py --model qwen3-asr-1.7b-hf --models-dir models
```
```bash
cd tauri/src-tauri && cargo build --release
```
模型也可以在设置窗口 → 模型里下载。

## 打开方式
- **双击** `tauri/src-tauri/target/release/localless.exe`
- **开机自启**：设置窗口 → 通用 → 开机启动

## 快捷键
| 操作 | 按键 |
|---|---|
| 听写（按住说话） | 右 Alt |

只有这一个，可以在设置窗口里改。除它之外不注册任何全局快捷键。

## 你的数据
设置、词语、听写历史、录音都只存在本机 `%APPDATA%\localless\`，**不在这个仓库里**，也不会上传到任何地方。
[`docs/localless-settings.example.json`](docs/localless-settings.example.json) 是一份通用设置模板，只用来看格式；第一次运行时程序会自己生成默认设置。

## 目录结构
- `tauri/src-tauri/`：Rust 壳，负责窗口、托盘、键盘钩子、剪贴板注入、历史库
- `tauri/src/`：界面，包括药丸、悬浮麦克风、设置页（打进二进制，改完要重新 `cargo build`）
- `app/engine.py`：本地识别服务，WebSocket 跑在 `127.0.0.1:8765`
- `app/*.ps1`：三个独立 sidecar，分别管键盘钩子、逐应用静音、读焦点控件
- `models/`：模型权重，不进 git

## 停止
托盘图标右键 → 退出。

## 测试
```bash
cd tauri/src-tauri && cargo test
```
```bash
node tauri/tests/pages.test.js && node tauri/tests/recorder.test.js && node tauri/tests/prompts.test.js
```
```bash
app/.venv/Scripts/python.exe -m unittest discover -s app -p "test_*.py"
```

## 许可
[CC BY-NC 4.0](https://creativecommons.org/licenses/by-nc/4.0/)：可以自由使用、修改、转发，但要署名，且不得用于商业用途。全文见 `LICENSE`。
模型权重不在本仓库里，各自遵循原作者的许可。
