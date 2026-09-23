import asyncio, ctypes, itertools, json, logging, os, re, struct, subprocess, sys, threading, time, uuid
from collections import deque
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from pathlib import Path

import numpy as np

ROOT = Path(__file__).resolve().parent.parent
MODELS = ROOT / "models"
ASR_DEFAULT_ID = "qwen3-asr-1.7b-hf"
ASR_DIR = MODELS / ASR_DEFAULT_ID
QWEN_GGUF = MODELS / "llm" / "Qwen3-4B-Instruct-2507-Q4_K_M.gguf"  # 只给「从修改中学词」用
VAD_ONNX = MODELS / "vad" / "silero_vad.onnx"  # optional (may 404); engine falls back to energy gate

SAMPLE_RATE = 16000
WS_HOST, WS_PORT = "127.0.0.1", 8765
SETTINGS_PATH = Path(os.environ.get("APPDATA", ".")) / "localless" / "localless-settings.json"
REGISTRY_PATH = Path(__file__).with_name("model-registry.json")
PROMPTS_PATH = Path(__file__).with_name("prompts.json")
IMPORTED_PATH = SETTINGS_PATH.parent / "imported-models.json"
LOG = logging.getLogger("engine")

AUDIO_FRAME_MAGIC = b"LLAF"
AUDIO_FRAME_VERSION = 1


def encode_audio_frame(audio_id: str, payload: bytes) -> bytes:
    aid = audio_id.encode("utf-8")
    if not aid or len(aid) > 255:
        raise ValueError("audio_id must encode to 1..255 bytes")
    return AUDIO_FRAME_MAGIC + bytes((AUDIO_FRAME_VERSION, len(aid))) + aid + payload


def decode_audio_frame(frame: bytes) -> tuple[str | None, bytes]:
    if not frame.startswith(AUDIO_FRAME_MAGIC):
        return None, frame
    if len(frame) < 6:
        raise ValueError("truncated tagged audio frame")
    version, aid_len = frame[4], frame[5]
    if version != AUDIO_FRAME_VERSION:
        raise ValueError(f"unsupported audio frame version {version}")
    end = 6 + aid_len
    if not aid_len or len(frame) < end:
        raise ValueError("truncated audio_id in tagged audio frame")
    return frame[6:end].decode("utf-8"), frame[end:]

def model_catalog() -> dict:
    items = json.loads(REGISTRY_PATH.read_text(encoding="utf-8"))["models"]
    try: items += json.loads(IMPORTED_PATH.read_text(encoding="utf-8"))
    except Exception: pass
    return {x["id"]: x for x in items}

def catalog_path(m: dict) -> Path:
    return Path(m.get("path")) if m.get("path") else MODELS / (m.get("entry") or m.get("installDir") or "")

# ---------------------------------------------------------------- audio utils

def wav_f32_to_pcm16(buf: bytes) -> bytes:
    """Convert a WAV (float32 format-tag 3 or int16 tag 1, any rate) to raw pcm16 mono 16k bytes."""
    import struct
    if buf[:4] != b"RIFF":
        raise ValueError("not a RIFF file")
    # manual parse — the stdlib wave module rejects IEEE-float (tag 3)
    pos, fmt, data = 12, None, None
    while pos + 8 <= len(buf):
        cid = buf[pos:pos + 4]
        size = struct.unpack_from("<I", buf, pos + 4)[0]
        body = buf[pos + 8:pos + 8 + size]
        if cid == b"fmt ":
            tag, ch, fr, _br, _ba, sw_bits = struct.unpack_from("<HHIIHH", body)
            fmt = (tag, ch, fr, sw_bits)
        elif cid == b"data":
            data = body
        pos += 8 + size + (size & 1)
    if fmt is None or data is None:
        raise ValueError("bad wav: missing fmt/data")
    tag, ch, fr, sw_bits = fmt
    if tag == 3 and sw_bits == 32:
        x = np.frombuffer(data, np.float32)
    elif tag == 1 and sw_bits == 16:
        x = np.frombuffer(data, np.int16).astype(np.float32) / 32768.0
    else:
        raise ValueError(f"unsupported wav tag={tag} bits={sw_bits}")
    if ch > 1:
        x = x.reshape(-1, ch).mean(axis=1)
    if fr != SAMPLE_RATE:
        g = np.gcd(fr, SAMPLE_RATE)
        from scipy.signal import resample_poly
        x = resample_poly(x, SAMPLE_RATE // g, fr // g)
    return (np.clip(x, -1, 1) * 32767).astype(np.int16).tobytes()


def frame_to_pcm16(buf: bytes, meta: dict) -> bytes:
    fmt = (meta.get("audio_format") or "wav").lower()
    if fmt == "wav" or buf[:4] == b"RIFF":
        return wav_f32_to_pcm16(buf)
    if fmt == "webm" or buf[:4] == b"\x1a\x45\xdf\xa3":
        return webm_to_pcm16(buf)
    # raw pcm s16le
    sr = int(meta.get("audio_sample_rate") or SAMPLE_RATE)
    ch = int(meta.get("audio_channels") or 1)
    x = np.frombuffer(buf, np.int16).astype(np.float32) / 32768.0
    if ch > 1:
        x = x.reshape(-1, ch).mean(axis=1)
    if sr != SAMPLE_RATE:
        g = np.gcd(sr, SAMPLE_RATE)
        from scipy.signal import resample_poly
        x = resample_poly(x, SAMPLE_RATE // g, sr // g)
    return (np.clip(x, -1, 1) * 32767).astype(np.int16).tobytes()


_ffmpeg = None
def webm_to_pcm16(buf: bytes) -> bytes:
    global _ffmpeg
    import subprocess, tempfile
    if _ffmpeg is None:
        import imageio_ffmpeg
        _ffmpeg = imageio_ffmpeg.get_ffmpeg_exe()
    with tempfile.NamedTemporaryFile(suffix=".webm", delete=False) as f:
        f.write(buf); src = f.name
    try:
        out = subprocess.run(
            [_ffmpeg, "-v", "error", "-i", src, "-ac", "1", "-ar", str(SAMPLE_RATE), "-f", "s16le", "-"],
            capture_output=True, check=True).stdout
        return out
    finally:
        os.unlink(src)

def mic_gain_db(value=None) -> float:
    """把麦克风增益夹到设置页支持的范围；无效值保持 0 dB。"""
    try:
        value = float(value)
    except (TypeError, ValueError, OverflowError):
        return 0.0
    if not np.isfinite(value):
        return 0.0
    return max(-12.0, min(30.0, value))


class AudioProcessor:
    """每段录音独占的有状态 DSP；输入输出都是 16 kHz 单声道 float32。"""
    def __init__(self, gain_db=0, noise_reduction=False):
        self.gain_db = mic_gain_db(gain_db)
        self.noise_reduction = bool(noise_reduction)
        self.linear_gain = 10.0 ** (self.gain_db / 20.0)
        cutoff_hz = 80.0
        dt = 1.0 / SAMPLE_RATE
        rc = 1.0 / (2.0 * np.pi * cutoff_hz)
        self.hp_alpha = rc / (rc + dt)
        self.hp_prev_x = 0.0
        self.hp_prev_y = 0.0
        self.noise_floor = 0.0005
        self.gate_open = False
        # 这段录音里门有没有开过。没开过之前不学底噪：见 _noise_gate。
        self.gate_ever_open = False
        self.gate_envelope = 0.06

    def _high_pass(self, x: np.ndarray) -> np.ndarray:
        out = np.empty_like(x, dtype=np.float32)
        prev_x, prev_y, alpha = self.hp_prev_x, self.hp_prev_y, self.hp_alpha
        for i, sample in enumerate(x):
            value = alpha * (prev_y + float(sample) - prev_x)
            out[i] = value
            prev_x, prev_y = float(sample), value
        self.hp_prev_x, self.hp_prev_y = prev_x, prev_y
        return out

    def _noise_gate(self, x: np.ndarray) -> np.ndarray:
        if not len(x):
            return x
        rms = float(np.sqrt(np.mean(np.square(x, dtype=np.float64))))
        open_at = max(0.0015, self.noise_floor * 3.2)
        close_at = max(0.0010, self.noise_floor * 2.0)
        self.gate_open = rms >= (close_at if self.gate_open else open_at)
        if self.gate_open:
            self.gate_ever_open = True
        # 只在门关闭时学习背景，避免把真正的人声抬成新的噪声底——但这一条只防得住
        # 门已经开过的情况。门从没开过时，我们对这支麦克风一无所知，手里唯一的判据
        # 就是「低于门槛」，而那正是出错的地方：被误判的人声被学进底噪，底噪一升门槛
        # 跟着升，门更开不了，越错越深。rec-8bf0 就这么整整 26 秒一次没开过。
        # 门开过一次之后底噪才有了参照系，学习才是安全的。在那之前一律用出厂值。
        if not self.gate_open and self.gate_ever_open:
            self.noise_floor = 0.97 * self.noise_floor + 0.03 * rms
        target = 1.0 if self.gate_open else 0.06
        tau = 0.008 if target > self.gate_envelope else 0.14
        step = 1.0 - np.exp(-1.0 / (SAMPLE_RATE * tau))
        env = self.gate_envelope
        gains = np.empty(len(x), np.float32)
        for i in range(len(x)):
            env += (target - env) * step
            gains[i] = env
        self.gate_envelope = float(env)
        return x * gains

    @staticmethod
    def _soft_limit(x: np.ndarray) -> np.ndarray:
        # 低于 0.95 完全透明；只平滑压缩真正可能溢出的峰值。
        mag = np.abs(x)
        over = mag > 0.95
        if np.any(over):
            x = x.copy()
            x[over] = np.sign(x[over]) * (
                0.95 + 0.05 * np.tanh((mag[over] - 0.95) / 0.05)
            )
        return np.clip(x, -1.0, 1.0).astype(np.float32, copy=False)

    def process(self, x: np.ndarray) -> np.ndarray:
        x = np.asarray(x, dtype=np.float32)
        if not len(x):
            return x.copy()
        if not self.noise_reduction and self.gain_db == 0:
            return x.copy()
        out = self._high_pass(x) if self.noise_reduction else x.copy()
        # 增益必须加在门之前。加在门之后的话，门判断时看到的永远是原始电平，用户把
        # 「麦克风增益」推到底也够不着门——而那个旋钮正是电平不够时唯一能动的东西。
        # 门槛里相对的那半截（noise_floor * 3.2）不受影响：底噪和人声一起被抬。绝对
        # 的那半截（0.0015）本来就是「人声至少这么响」的假设，信号被抬起来之后这个
        # 假设只会更成立。顺序只改变门看到什么，增益和门增益都是乘法，结果等价。
        out = out * self.linear_gain
        if self.noise_reduction:
            out = self._noise_gate(out)
        # 这里**不**限幅，也不夹到 ±1：整段的峰值要等录完才知道，见 normalize。
        return out

    def normalize(self, x: np.ndarray) -> np.ndarray:
        """整段录完之后压一次电平。限幅从「每块各削各的」变成只兜底。

        限幅器原来跟在 process() 里，也就是每来一块音频就削一次。对安静的麦
        永远不触发（PD100X 峰值 0.038，×3.162 = 0.12，离 0.95 远得很），可
        micGainDb 是个固定倍数：远程会话里的 UU 虚拟麦本身就顶着满刻度出来
        （峰值 1.000），再 ×3.162 就有 8.76% 的采样点被压进 0.95 那条平台。
        人耳听回放没事（存下来的 wav 是增益前的），模型那边是 17.9 秒音频
        188ms 吐空——rec-beb1 就是这么丢的，重转三遍都是空。

        用同一条录音跑增益梯度量出来的边界（左边是喂给模型的 rms）：

            0.0630 / 0.1050 / 0.1993  →  937~1063ms，整句转出
            0.2695 / 0.3116 / 0.3714  →  188~266ms，一个字都没有

        注意 0.1993 那一档就是原文件本身，一个字没改照样转得出来。所以毁掉它的
        不是电平高，是限幅削出来的失真——0.2695 那一档峰值同样是 1.000，区别只在
        被削掉多少。

        修法就是把限幅挪到整段都在手里之后：先按整段峰值统一缩到 0.95，限幅器
        于是几乎碰不到，缩放本身又是个纯标量，不带任何失真。安静的麦这条路和
        以前一字不差（0.12 < 0.95，直接原样返回）。

        别改回流式。中途试过「边录边按已见过的最大峰值封顶」，结果是同一句话里
        前 1.8 秒 ×3.02、后面 ×0.95，硬生生插进一个 10 dB 的台阶，照样转不出来。
        峰值这个数必须等整段听完。
        """
        if not len(x):
            return np.zeros(0, np.float32)
        peak = float(np.abs(x).max())
        if peak > 0.95:
            x = x * (0.95 / peak)
        return self._soft_limit(x)


# ------------------------------------------------------------ user settings

def _strip_text_slot(s: str) -> str:
    """比对旧默认时忽略末尾的「转写原文：{{text}}」——这块是占位槽不是措辞，
    加没加它渲染结果一样，不该因此认定用户自定义过。"""
    return _TEXT_SLOT_RE.sub("", str(s or "")).strip()


def migrate_prompts(st: dict) -> dict:
    """保存值逐字等于某条旧默认 = 用户从没自定义过 → 丢掉，让新默认生效。
    用户真改过一个字就原样保留，绝不覆盖。"""
    for key, olds in LEGACY_PROMPTS.items():
        saved = st.get(key)
        if isinstance(saved, str) and any(_strip_text_slot(saved) == _strip_text_slot(o) for o in olds):
            st.pop(key, None)
    return st


def load_settings() -> dict:
    """UI 设置窗口写来的 JSON；每次 finalize 前重读，改动即时生效。"""
    try:
        return migrate_prompts(json.loads(SETTINGS_PATH.read_text(encoding="utf-8")))
    except Exception:
        return {}


def setting_words(st: dict) -> list[str]:
    if not st.get("customWordsEnabled", True):
        return []
    return [str(x.get("text") if isinstance(x, dict) else x) for x in (st.get("customWords") or []) if (x.get("text") if isinstance(x, dict) else x)]


def words_by_tag(st: dict) -> str:
    """自定义词语按分组列出，让模型知道"这是人名 / 这是项目名"。"""
    if not st.get("customWordsEnabled", True):
        return ""
    names = {g["id"]: g.get("name") or g["id"] for g in (st.get("wordTags") or []) if isinstance(g, dict) and g.get("id")}
    groups: dict[str, list[str]] = {}
    for w in st.get("customWords") or []:
        if not isinstance(w, dict) or not w.get("text"):
            continue
        groups.setdefault(names.get(w.get("tag"), "通用"), []).append(str(w["text"]))
    return "; ".join(f"{k}: {', '.join(v)}" for k, v in groups.items() if v)


# ---------------------------------------------------------------- 设备档位

# 引擎自己把模型从内存搬进显存之前，空闲显存要比 stage_estimate_mb 算出来的
# 总量再多出这么多。
#
# 注意这**不是**用来盖激活值和 KV cache 的——stage_estimate_mb 每一档自己已经
# 加过了（asr 是权重×1.15+512，llm 是权重×1.15+1400）。这里要盖的是另一件事：
#
# 真正卡死这个值的是「别升上去马上又掉下来」。搬进显存之后剩余的空闲量大约就是
# 这个余量本身，而低于 ASR_FREE_FLOOR_MB 就会立刻触发降舱。所以它**必须明显大于
# ASR_FREE_FLOOR_MB**，否则引擎刚搬完就掉头搬回来，权重在内存和显存之间反复横跳，
# 比一直待在内存里还慢得多。2200 比 1400 的地板高出 800 MB，这 800 MB 同时也是
# 留给 CUDA 上下文、显存碎片，以及两次采样之间别的程序又抢回去一点的缓冲。
# test_device.py 里有一条断言守着这个不等式，改小之前先看那条测试。
GPU_PROMOTE_HEADROOM_MB = 2200

# 连着这么多次采样都够，才真的动手搬。显存是别的程序（ComfyUI 之类）在占，
# 它出完一张图的间隙会短暂空出一大块；单次采样撞上那个瞬间就搬家，下一秒又被
# 抢回去，于是权重在内存和显存之间来回搬，比一直待在内存里还慢。
GPU_PROMOTE_SAMPLES = 3

# 这里曾经有个 600 秒的冷静期，删掉了。它想防的是"刚被显存坑过就立刻搬回去"，
# 可它防的那件事根本不会发生：降舱线 ASR_FREE_FLOOR_MB(1400) 和升舱线 need(约
# 7 GB) 之间隔着 5 GB 以上的滞回带，而 need 里**已经算进了我们自己那份权重**——
# free >= need 成立就意味着卸载之后卡上真的装得下我们再加余量，抖不起来。
# 它实际造成的是：日志里每一次升舱都精确落在降舱后 10.5 分钟，那一刻空闲显存有
# 13.8~14.8 GB，早就够了。ComfyUI 出完图两分钟就把显存吐了出来，我们却还要在
# 内存里把剩下八分钟慢腾腾耗完。挡路的从来不是显存，是这个常数。
# 现在唯一的闸门是 vote()：连续 GPU_PROMOTE_SAMPLES 次采样都够才搬。

GPU_MONITOR_S = 20

# GPU 崩过一次的标记文件。看门狗在杀进程之前写下它，main.js 把引擎重新拉起来
# 之后，新进程读到就先用内存跑。
# 不留这个标记的话，重启只是让引擎一头撞回同一堵墙：显存还被占着，加载还是卡死，
# 看门狗还是杀进程。用户看到的是听写反复失败，而不是"慢一点但能用"。
GPU_DEMOTE_MARKER = ROOT / "gpu-demoted"
# 标记的保质期。显卡被占是暂时的，过了这段时间就当它已经缓过来了，重新允许升舱。
GPU_DEMOTE_TTL_S = 1800


class DeviceState:
    """模型此刻该放显存还是放内存，以及引擎能不能自己改主意。

    两个开关分工不同：
      * cpuOnlyEnabled  —— 用户手动选的档位，同时也是引擎的起步档位；
      * autoDeviceEnabled —— 允不允许引擎在运行时自己改主意。把它关掉，行为
        和以前完全一样：cpuOnlyEnabled 说了算，谁也不许动。

    引擎改主意只存在内存里，**绝不回写设置文件**。用户在设置页看到的永远是自己
    选的那个，不会哪天打开发现开关自己动了；重启引擎也自然回到用户的选择。
    """

    def __init__(self):
        # None = 跟随设置；"cpu"/"gpu" = 引擎自己定的，压过设置。
        self._override: str | None = None
        self._reason = ""
        self._streak = 0
        self._seen: bool | None = None
        # cpu_only() 在推理路径上被高频调用，而这些字段由监测协程改。
        # 加把锁，免得读到写了一半的状态。
        self._lock = threading.Lock()

    def auto(self) -> bool:
        return bool(load_settings().get("autoDeviceEnabled", True))

    def cpu_only(self) -> bool:
        manual = bool(load_settings().get("cpuOnlyEnabled"))
        if not self.auto():
            return manual
        with self._lock:
            # 用户在设置页动了开关：那是明确指令，把引擎自己的判断作废。
            # 不这么做的话，用户点了「用内存跑模型」却发现没反应——引擎的
            # override 还压在上面。那比不做这个功能更糟。
            if self._seen is not None and self._seen != manual:
                self._override, self._streak = None, 0
            self._seen = manual
            return manual if self._override is None else self._override == "cpu"

    def on_gpu(self) -> bool:
        return not self.cpu_only()

    def vote(self, enough: bool) -> bool:
        """监测每采样一次投一票；连续够了 GPU_PROMOTE_SAMPLES 次才算数。"""
        with self._lock:
            if not enough:
                self._streak = 0
                return False
            self._streak += 1
            return self._streak >= GPU_PROMOTE_SAMPLES

    def demote(self, reason: str) -> bool:
        """转回内存跑。返回 False 表示用户关了自动切换，引擎不该擅自改。"""
        if not self.auto():
            return False
        with self._lock:
            # 连击清零就够了：显存真空出来还得连续投够票才搬回去，不用再压一段
            # 冷静期（理由见 GPU_PROMOTE_SAMPLES 上面那段）。
            self._streak = 0
            first = self._override != "cpu"
            self._override, self._reason = "cpu", reason
        if first:
            LOG.warning("显卡不好使了（%s）——转用内存跑，听写继续", reason)
        return True

    def promote(self, reason: str) -> bool:
        if not self.auto():
            return False
        with self._lock:
            if self._override == "gpu":
                return False
            self._override, self._reason = "gpu", reason
            self._streak = 0
        # 崩过的标记要跟着作废，否则下次重启还按"刚崩过"起步。
        try:
            GPU_DEMOTE_MARKER.unlink(missing_ok=True)
        except Exception:
            pass
        LOG.info("显存腾出来了（%s）——把模型搬回显卡", reason)
        return True

    def snapshot(self) -> dict:
        """给托盘看的：此刻究竟在哪、以及为什么。

        `at` 是**实际生效**的档位，不是用户选的那个——引擎自动降舱之后两者会不
        一致，而托盘要显示的正是实际值。`manual` 才是设置里那个开关。
        两个都给，托盘才分得清「你选了显存但现在跑在内存上」这种状态。

        故意复用 cpu_only() 而不是直接读 _override：那个方法里还有一段
        「用户动过设置就作废引擎的判断」的逻辑，绕过去的话托盘会读到已经该失效
        的 override。托盘每几秒问一次，等于顺便让这段逻辑跑得更勤，是好事。
        """
        at_cpu = self.cpu_only()
        auto = self.auto()
        with self._lock:
            override, reason = self._override, self._reason
        # 关掉自动切换时 cpu_only() 直接返回用户选的那个，override 根本不参与，
        # 所以这里也不能报"引擎改的"——那会让托盘对着一个没在生效的 override
        # 解释半天，而实际档位明明就是用户自己选的。
        forced = auto and override is not None
        return {
            "at": "cpu" if at_cpu else "gpu",
            "manual": "cpu" if bool(load_settings().get("cpuOnlyEnabled")) else "gpu",
            "auto": auto,
            # 引擎自己改过主意、而且那个判断确实在生效，才为真。托盘用它决定要不要
            # 把原因显示出来——用户点了显存却看到「内存」，不给原因就只会觉得开关坏了。
            "forced": forced,
            "why": reason if forced else "",
        }

    def note_gpu_death(self, reason: str):
        """看门狗马上要杀进程了，留个标记给重启后的新进程。"""
        try:
            GPU_DEMOTE_MARKER.write_text(
                json.dumps({"at": time.time(), "why": reason}, ensure_ascii=False),
                encoding="utf-8")
        except Exception:
            pass

    def adopt_marker(self):
        """启动时读上一条命留下的标记，接着用内存跑。"""
        try:
            data = json.loads(GPU_DEMOTE_MARKER.read_text(encoding="utf-8"))
            age = time.time() - float(data.get("at", 0))
            why = str(data.get("why") or "上次 GPU 卡死")
        except Exception:
            return
        # 时间倒流（改过系统时间）也当过期：宁可多试一次显卡，也别永远锁在内存里。
        if age < 0 or age > GPU_DEMOTE_TTL_S:
            try:
                GPU_DEMOTE_MARKER.unlink(missing_ok=True)
            except Exception:
                pass
            return
        if self.demote(f"上次因「{why}」被杀，先用内存起步"):
            LOG.warning("检测到 %.0f 秒前 GPU 卡死过，本次先用内存跑；显存空出来会自动搬回去", age)


DEVICE = DeviceState()


def cpu_only() -> bool:
    """把模型全部放进内存跑。显卡被别的程序（ComfyUI 之类）占满时，
    与其排队等显存、或者被 WDDM 挤到共享内存里慢上百倍，不如老实用 CPU。
    每次加载时重读设置，改完开关下一次加载就生效。

    现在这个答案还可能由引擎自己改（见 DeviceState）：显存空出来了会搬回显卡，
    显卡崩了会退回内存。用户想要绝对的手动控制就关掉「自动切换设备」。
    """
    return DEVICE.cpu_only()


def is_gpu_failure(exc: BaseException) -> bool:
    """这个异常是不是「显卡不好使了」。

    显存耗尽在各后端里长相完全不同：torch 抛 torch.cuda.OutOfMemoryError，
    llama.cpp 只是普通 RuntimeError 里带一句 failed to allocate，ct2 又是别的字样。
    与其逐个 import 它们的异常类（还得处理没装那个包的情况），不如认字符串。
    认错了最多多降一次级、慢一点；认漏了就是用户的听写直接失败——两种代价不对等，
    所以这里故意放宽。
    """
    text = f"{type(exc).__name__}: {exc}".lower()
    return any(k in text for k in (
        "out of memory", "outofmemory", "cuda error", "cuda_error", "cublas", "cudnn",
        "no kernel image", "device-side assert", "failed to allocate",
        "unspecified launch failure", "invalid device", "no cuda-capable",
    ))


def asr_resident(st: dict) -> bool:
    """ASR 权重要不要一直留在显存里。

    默认开。qwen3-asr-1.7b 半精度约 3.4 GB，和精修模型（8B 的 Q4 约 5.5 GB）
    一起放在 16 GB 卡上绰绰有余，换来的是每次听写前省掉约 1.9 s 冷加载——
    开机第一次说话的等待感基本就是这一段。
    显卡还要跑别的（ComfyUI 之类）时关掉它，退回原来「用完就卸」的行为。
    """
    return bool(st.get("asrKeepLoaded", True))


def learn_model_path(st: dict) -> Path:
    """「从修改中学词」用的模型。

    听写主流程已经不碰 LLM 了，整条精修/Bool/Thinking 链路连同它们的角色槽一起
    删掉，全仓库只剩这一件事还需要一个语言模型：判断一次改动值不值得存成词条，
    并把词和分组提出来。用户点一下才跑的一次性活，冷加载就够，不预热也不常驻。
    """
    meta = model_catalog().get(st.get("learnModel") or "qwen3-4b")
    if meta and meta.get("category") == "llm" and meta.get("backend") == "llama.cpp":
        p = catalog_path(meta)
        if p.exists():
            return p
    LOG.warning("学词模型不可用，回退 Qwen3-4B")
    return QWEN_GGUF




VAR_LABELS = {
    "wordsByTag": "【自定义词表（按类别）】仅作写法参照，听到同音时按这里的写法输出，没说到的词绝不主动写进去：\n",
    "focused": ("【光标所在输入框里已有的文字，只读】这段文字**已经在屏幕上了**，"
                "你的输出会被粘贴到它后面。只拿它对齐语气、术语和当前语言；"
                "禁止把它的任何一句原样或改写后写进输出，禁止复述、翻译、回答或总结它。"
                "输出**只含本次转写**，一个字都不许多：\n"),
    "history": ("【用户最近几条听写记录，从旧到新】只用来参照人名与专有名词的写法，"
                "以及帮助判断本次转写里的代词和省略指代什么。它们已经发出去了，"
                "绝不可重复、合并或续写进本次输出：\n"),
    "text": ("========\n以上全部是规则与参考资料，一个字都不要输出。\n"
             "下面【转写原文】里的内容才是你唯一要整理的输入，只输出它整理后的结果：\n"
             "转写原文：\n"),
}

# wordsByTag 属 ASR 侧偏置，精修模型不需要；要用可在模板里显式写 {{变量}}。
# corrections 已由「词语自学习」取代，不再作为上下文标签存在。
AUTO_VARS = ("focused", "history", "text")


_VAR_RE = re.compile(r"\{\{\s*(\w+)\s*\}\}")
# 补充规则（自我改正 / 去重 / 已有词表）的插入点。模板里没写就退回旧的兜底锚点。
_RULES_SLOT_RE = re.compile(r"\{\{\s*rules\s*\}\}")
# 模板里「转写原文：\n{{text}}」整块摘掉——转写原文由 build_prompt 统一放到最末尾
_TEXT_SLOT_RE = re.compile(r"[ \t]*转写原文[:：][ \t]*\n?\s*(\{\{\s*text\s*\}\}|\{text\})?")


def prompt_vars(st: dict, text="", history=None, corrections=None,
                focused_text="", target_app="", lang="", wrong="", right="") -> dict:
    words = ", ".join(setting_words(st))
    tagged = words_by_tag(st)
    return {
        "text": text,
        # 规则由 _with_rule 插在槽位「之前」，槽位本身永远渲染成空串。
        "rules": "",
        "words": words,
        "wordsByTag": tagged,
        # 输入框内容和听写历史无条件送进来，用不用由模板里写不写 {{focused}}
        # / {{history}} 决定——比一个布尔开关表达力强，也不会出现「开关关着
        # 但模板引用了变量」这种自相矛盾的状态。截断留着：500/300 不是开关，
        # 是预算，上下文再有用也不能把提示词顶过长度上限（那会让 ASR 吐空）。
        "focused": (focused_text or "")[-500:],
        "history": "\n".join(str(x)[-300:] for x in (history or [])[-20:]),
        "corrections": "\n".join(f"{x.get('wrong','')} → {x.get('right','')}" for x in (corrections or [])[:30]),
        "app": target_app or "",
        "lang": lang or "",
        "wrong": wrong or "",
        "right": right or "",
        "tags": ", ".join(str(g.get("name") or g.get("id")) for g in (st.get("wordTags") or []) if isinstance(g, dict)),
    }


def render_prompt(tpl: str, v: dict) -> str:
    """{{var}} 是主语法；{var} 为兼容旧模板保留。未知变量渲染为空串，不报错。"""
    out = _VAR_RE.sub(lambda m: str(v.get(m.group(1), "")), tpl or "")
    for k, val in v.items():
        out = out.replace("{" + k + "}", str(val))
    return out


def build_prompt(tpl: str, v: dict) -> str:
    """模板没显式引用的上下文变量按 AUTO_VARS 顺序追加，【转写原文】无条件收尾。

    模型只把 prompt 最后一块当输入。上下文排在转写原文后面时它会转去「整理」那块
    上下文，把提示词本身原样吐出来。所以 text 不参与排序，模板里写在中间的
    {{text}} 也会被挪到末尾。
    """
    tpl = tpl or ""
    used = set(_VAR_RE.findall(tpl)) | {k for k in v if "{" + k + "}" in tpl}
    seq = [k for k in AUTO_VARS if k != "text"]
    # ponytail: 只清「转写原文：」这一种自带标签，用户自造的标签留着当废话，不值得写解析器
    body = render_prompt(_TEXT_SLOT_RE.sub("", tpl), {**v, "text": ""}).strip()
    refs = [VAR_LABELS[k] + str(v[k]) for k in seq if k not in used and v.get(k)]
    tail = [VAR_LABELS["text"] + str(v["text"])] if str(v.get("text") or "").strip() else []
    return "\n\n".join(p for p in [body] + refs + tail if p and p.strip())


NO_THINK_SYSTEM = ("你是一个严谨的文本整理器。禁止思考、禁止输出 <think> 或任何推理过程，"
                   "直接给出整理后的正文，不解释、不寒暄。 /no_think")

# 输入框里已经有的字，用户往往会顺口再念一遍当引子（框里写着"我想把"，嘴上也说
# "我想把…"）。照抄进去就成了"我想把我想把…"。两个字符串都是已知的，精确重叠
# 一比就知道，用代码删就行——原先还有一条让模型去判的补充规则，4B 只做对一半、
# 还会照抄示例里的字，随精修链路一起删了。
ECHO_MIN_CHARS = 2       # 少于这个长度的重叠多半是巧合（"的""了"）
ECHO_MAX_CHARS = 40      # 引子不会太长，限一下避免误删整句


def strip_focused_echo(focused: str, text: str) -> str:
    """删掉转写开头与输入框结尾精确重叠的那一截。只认精确重叠，绝不模糊匹配——
    宁可漏删，也不能吃掉用户真正说的话。"""
    a, b = (focused or "").rstrip(), (text or "").lstrip()
    if not a or not b:
        return text
    for k in range(min(len(a), len(b), ECHO_MAX_CHARS), ECHO_MIN_CHARS - 1, -1):
        if a.endswith(b[:k]):
            rest = b[k:].lstrip(" 　,，.。、")
            # 整句都是重复时保持原样：与其输出空字符串，不如让用户看到重复
            return rest or text
    return text



def existing_words_rule(st: dict, tpl: str = "") -> str:
    """词表里已经有的词就别再提了。客户端会兜底去重，但模型不知情的话每次都会
    白跑一趟判定，还会弹一次"已添加"的提示——用户以为又学了个新词。

    模板里已经写了 {{words}} / {{wordsByTag}} 就不再追加：那说明用户自己决定了
    词表放在哪、怎么措辞，轮不到这里硬塞一段看不见也改不了的规则。
    """
    if re.search(r"\{\{?\s*(words|wordsByTag)\s*\}?\}", tpl or ""):
        return ""
    words = setting_words(st)
    if not words:
        return ""
    listed = "、".join(words[:200])
    return ("\n补充规则：下面这些词**已经在词表里了**，绝对不要再提交它们（大小写、"
            "空格不同也算同一个词）：" + listed +
            "\n只有当改后的文本里出现了上面没有的新专名时，才回 add:true。")


def _with_rule(tpl: str, rule: str) -> str:
    """把补充规则插进指令区。

    首选 {{rules}} 槽位：默认模板把它放在「要做的事」之后、「不要做的事」之前，
    这些规则本来就该待在那儿。以前没有槽位时靠 _TEXT_SLOT_RE 兜底，而默认模板写的是
    【转写原文】（方括号、无冒号）根本匹配不上，规则于是被追加到收尾句之后，读起来
    像是提示词已经结束了才补的一句闲话——自我改正规则长期不生效就是这么来的。
    插在槽位之前而不是替换掉它，多条规则才能依次插入；空槽最后由 render_prompt 渲染成空串。

    绝不能追加在转写原文后面——模型只把 prompt 最后一块当输入，规则排在那儿会被
    当成待整理的正文照抄进输出（build_prompt 的注释早写明了这个坑，我照样踩了一次）。
    """
    if not rule:
        return tpl
    # 【转写原文】是次选锚点；自学习那种没有转写原文的模板就退到内容段之前。
    slot = _RULES_SLOT_RE.search(tpl or "")
    hit = slot or _TEXT_SLOT_RE.search(tpl or "") or re.search(r"改之前[:：]", tpl or "")
    if hit:
        # 槽位那行渲染后自己会留一个换行，再补两个就多出一条空行。
        return tpl[:hit.start()] + rule.strip() + ("\n" if slot else "\n\n") + tpl[hit.start():]
    return (tpl or "") + rule

def build_learn_prompt(st: dict, wrong: str, right: str) -> str:
    """「从修改中学词」的提示词。词表里已经有的词由 existing_words_rule 挡在外面，
    否则模型每次都会把老词再交一遍，用户以为又学了个新词。"""
    tpl = st.get("learnPrompt") or DEFAULT_PROMPTS.get("learnPrompt") or ""
    tpl = _with_rule(tpl, existing_words_rule(st, tpl))
    return render_prompt(tpl, prompt_vars(st, wrong=wrong, right=right))


def asr_prompt(st: dict, focused_text="") -> str | None:
    """渲染发给 ASR 的 system 段。

    这里**不吃** corrections。以前它把每条手动修改的 right（改后的**整句**）
    当热词塞进词表：30 条整句能把上下文顶到两千多字，再被 clip_qwen_ctx 从尾巴
    一刀切——于是提示词里全是用户自己以前说过的话，真正的词表反倒被挤掉了。
    值得进词表的专名，「词语自学习」（learnPrompt）已经提取成 customWords，
    走 setting_words 这条正路进来，这里再塞一遍纯属重复且有害。
    这跟「听写历史」开关无关：Qwen3-ASR 的上下文格是热词位，塞整段历史只会把它
    顶过长度上限直接吐空，所以历史从来没进过 ASR 提示词。
    """
    tagged = words_by_tag(st)
    words = list(dict.fromkeys(x for x in setting_words(st) if x))
    if not (words or focused_text):
        return None
    tpl = st.get("asrPrompt") or DEFAULT_PROMPTS["asrPrompt"]
    ctx = tagged or ", ".join(words)
    focused = (focused_text or "")[-200:]
    # 预算是给整段 system 的，但会撑爆它的只有词表。这里先把固定部分（规则、
    # 输入框上下文）渲染一遍量出占用，剩下的额度才给词表——否则等到
    # _qwen_inputs 里 clip_qwen_ctx 从尾巴一刀切，砍掉的正是词表末尾的自定义词，
    # 而开头那几行规则毫发无损。规则是几十个字的常量，词表才是会长的那头。
    fixed = render_prompt(tpl, {"words": "", "wordsByTag": "", "focused": focused})
    room = qwen_ctx_limit(st.get("qwenCtxMaxChars")) - len(fixed)
    if room > 0:
        ctx = clip_qwen_ctx(ctx, room)
    return render_prompt(tpl, {"words": ", ".join(words), "wordsByTag": ctx,
                               "focused": focused}).strip() or None


# ---------------------------------------------------------------- GPU admission

class GpuMemoryProvider:
    """Read live NVIDIA memory. Values are bytes; None means diagnostics unavailable."""
    def __init__(self):
        self._nvml = None
        self._device = None
        self._init_nvml()

    def _init_nvml(self):
        for name in ("nvml.dll", "nvml64.dll"):
            try:
                dll = ctypes.WinDLL(name)
                if dll.nvmlInit_v2() != 0:
                    continue
                device = ctypes.c_void_p()
                if dll.nvmlDeviceGetHandleByIndex_v2(0, ctypes.byref(device)) == 0:
                    self._nvml, self._device = dll, device
                    return
            except Exception:
                continue

    def sample_sync(self) -> tuple[int, int] | None:
        if self._nvml is not None:
            class Memory(ctypes.Structure):
                _fields_ = [("total", ctypes.c_ulonglong), ("free", ctypes.c_ulonglong),
                            ("used", ctypes.c_ulonglong)]
            info = Memory()
            try:
                if self._nvml.nvmlDeviceGetMemoryInfo(self._device, ctypes.byref(info)) == 0:
                    return int(info.total), int(info.free)
            except Exception:
                pass
        try:
            out = subprocess.run(
                ["nvidia-smi", "--query-gpu=memory.total,memory.free", "--format=csv,noheader,nounits", "-i", "0"],
                capture_output=True, text=True, timeout=3, check=True).stdout.splitlines()[0]
            total_mb, free_mb = (int(x.strip()) for x in out.split(",")[:2])
            return total_mb * 1024 * 1024, free_mb * 1024 * 1024
        except Exception:
            return None

    async def sample(self) -> tuple[int, int] | None:
        return await asyncio.to_thread(self.sample_sync)

    # NVML_ERROR_INSUFFICIENT_SIZE。只问个数（buf 传 None）时，驱动可能拿它表示
    # "数组不够大"，也可能直接返回成功——两种都算问到了，别把它当失败。
    _NVML_INSUFFICIENT_SIZE = 7

    def encoder_pids_sync(self) -> list[int] | None:
        """正在用显卡编码视频的进程 pid。

        `None` = 问不出来（没 N 卡、NVML 不通）；`[]` = 确实没有人在编码。
        **这两个必须分开**：调用方拿它判断"是不是正在远程串流"，前者该往
        "显示悬浮麦克风"倒，后者该藏起来，混成一个值就等于把 fail-open 拆了。

        为什么是这条信号：UU 远程（网易 GameViewer）推流时靠 NVENC 编码画面，
        整个编码在显卡上做完，所以它在主机上几乎不留别的痕迹——SM_REMOTESESSION
        认不出（推的是控制台会话）、进程 CPU 增量是 0.00s、日志速率在 2026-09-09
        换成二进制 .slog 之后推流反而比空闲还低。编码会话是唯一咬得住的。

        这里只报**原始事实**（谁在编码）。"这算不算远程会话"是 main.js 的事：
        它手上有推流日志的文件名，末尾就是那个 GameViewerServer 的 pid。
        """
        if self._nvml is None:
            return None

        class Session(ctypes.Structure):
            # nvmlEncoderSessionInfo_t：8 个 unsigned int，顺序不能动。
            _fields_ = [("sessionId", ctypes.c_uint), ("pid", ctypes.c_uint),
                        ("vgpuInstance", ctypes.c_uint), ("codecType", ctypes.c_uint),
                        ("hResolution", ctypes.c_uint), ("vResolution", ctypes.c_uint),
                        ("averageFps", ctypes.c_uint), ("averageLatency", ctypes.c_uint)]

        try:
            count = ctypes.c_uint(0)
            rc = self._nvml.nvmlDeviceGetEncoderSessions(self._device, ctypes.byref(count), None)
            if rc not in (0, self._NVML_INSUFFICIENT_SIZE):
                return None
            if count.value == 0:
                return []
            buf = (Session * count.value)()
            if self._nvml.nvmlDeviceGetEncoderSessions(self._device, ctypes.byref(count), buf) != 0:
                return None
            # count 可能被第二次调用改小（会话正好结束），按它为准。
            return [int(buf[i].pid) for i in range(min(count.value, len(buf)))]
        except Exception:
            return None

    async def encoder_pids(self) -> list[int] | None:
        return await asyncio.to_thread(self.encoder_pids_sync)


# 一份就够：NVML 句柄是进程级的，调度器、听写前的预检和看门狗共用同一个，
# 免得每处各自 WinDLL 一遍。
GPU_PROVIDER = GpuMemoryProvider()


def free_vram_mb() -> int | None:
    """当前空闲显存（MB）。None = 不该按显存来判断。

    两种情况都归到 None，因为调用方要做的事是一样的——别拦。
      * 测不出来：没有 N 卡，NVML 和 nvidia-smi 都不通；
      * 纯 CPU 模式：模型根本不进显存。这时候 ComfyUI 占了多少跟我们没关系，
        真按余量去卡，就会在一台能好好干活的机器上拒绝听写。
    """
    if cpu_only():
        return None
    sample = GPU_PROVIDER.sample_sync()
    return None if sample is None else sample[1] // 1024 // 1024


# ---------------------------------------------------------------- 卡死看门狗

# 显存低于这条线就不再让 ASR 和 LLM 同时常驻。
#
# 起因是一次实测的彻底卡死：ComfyUI 占了 5.0 GB，本进程自己常驻 10.1 GB
# （8B 的 Q4 精修模型约 5.5 GB + 1.7B 的 ASR 半精度约 3.4 GB + CUDA 上下文），
# 16376 MB 的卡上只剩 256 MB。一段 **1.0 秒** 的音频送进 generate()，九分钟没回来，
# py-spy 抓到线程停在 _prefill 的 sdpa_attention_forward 里。
#
# 关键在于这时候 **不会** 抛 CUDA OOM：Windows 的 WDDM 在显存耗尽时会把显存往
# 系统内存里挪来兜底，于是分配不是失败而是慢到没有尽头。所有按"会抛异常"设计的
# 保护因此全部落空——整份日志里连一行错误都没有。
ASR_FREE_FLOOR_MB = 1400
# 正常一次听写 3~5 秒（实测 57.9 秒音频用了 5.3 秒），冷加载权重约 6 秒。
# 这两条线取的是"再慢也不该到"的量级，不是性能指标。
ASR_WATCH_S = 90
LOAD_WATCH_S = 240


def asr_budget(dur_s: float) -> float:
    """一段 dur_s 秒的音频，允许 ASR 跑多久。

    这里必须随音频长度放大。原先是写死的 60 秒，于是"说得越长越容易被判成
    卡死"——2026-09-05 一段 161.8 秒的听写就是这么丢的：CPU 上实测速度约
    0.3~0.7 倍音频时长（10.2s→7.4s、72.2s→24.1s），那段大概需要 55~90 秒，
    远在 60 秒之外，但它一直在正常干活。
    倍率给到 3 倍是因为这条线只是最后一道兜底，宁可松，也别再把一段正在好好
    转写的长音频掐掉。
    """
    return max(60.0, 20.0 + 3.0 * dur_s)

_watch_seq = itertools.count()
_watch_lock = threading.Lock()
_watch_jobs: dict[int, tuple[float, float, str]] = {}   # key -> (开始时刻, 上限秒数, 标签)


@contextmanager
def gpu_watchdog(label: str, timeout_s: float):
    """卡在 CUDA 里的活儿超时就让整个进程退出。

    进程内没有干净的救法，这不是偷懒：
      * Python 没有终止线程的手段，卡在原生调用里的工作线程杀不掉；
      * asyncio.wait_for 只是放弃等待，原生调用照跑——实测那圈 60 秒的
        wait_for 一次都没响过，因为 GIL 被卡住的调用攥着，事件循环连定时器
        都跑不了；
      * ASR 走的是 max_workers=1 的线程池，那具尸体会把之后每一次听写都堵在
        队列里，于是"卡了一次 = 永远卡着"，只能手动杀进程。
    退出进程反而是最干净的：main.js 的 startEngine 在 on('exit') 里会把它重新
    拉起来，权重重新加载，CUDA 上下文重建，客户端那边 ws 断开也会自己解锁。

    纯 CPU 模式下整个不设防。这里的上限是按显卡的速度定的，而 CPU 上同一段音频
    要慢一个量级——长音频超过 90 秒是正常速度，不是卡死。真按这个尺子去量，
    只会在用户说完一段长话之后把进程杀掉，语音直接丢了。而且 CPU 上本来也没有
    要防的那个东西：卡死的根源是 WDDM 在显存耗尽时不报错只挪内存，内存里没有
    这一出，真的爆了会老实抛 MemoryError。
    """
    if cpu_only():
        yield
        return
    key = next(_watch_seq)
    with _watch_lock:
        _watch_jobs[key] = (time.monotonic(), timeout_s, label)
    try:
        yield
    finally:
        with _watch_lock:
            _watch_jobs.pop(key, None)


def _watchdog_loop():
    while True:
        time.sleep(2)
        now = time.monotonic()
        with _watch_lock:
            overdue = [(started, timeout_s, label)
                       for started, timeout_s, label in _watch_jobs.values()
                       if now - started > timeout_s]
        if not overdue:
            continue
        started, timeout_s, label = overdue[0]
        LOG.error("%s 卡了 %.0f 秒没返回（上限 %.0f 秒），判定 GPU 已经卡死；"
                  "退出进程让 main.js 重载模型", label, now - started, timeout_s)
        try:
            LOG.error("退出前的显存：%s", GPU_PROVIDER.sample_sync())
        except Exception:
            pass
        # 留个标记给重启后的新进程，让它先用内存跑。
        # 不留的话重启就是一头撞回同一堵墙：显存还被占着，加载还是卡死，看门狗
        # 还是杀进程——用户看到的是听写反复失败，而不是"慢一点但能用"。
        DEVICE.note_gpu_death(label)
        # 必须先把日志刷干净：os._exit 不跑 atexit，缓冲区里的行会跟着进程一起没。
        logging.shutdown()
        # 用 _exit 而不是 sys.exit：现在有线程卡在驱动里，正常退出会在等它 join
        # 的时候一起挂住，那就白检测了。
        os._exit(3)


def start_watchdog():
    threading.Thread(target=_watchdog_loop, name="gpu-watchdog", daemon=True).start()


# 心跳文件：给 main.js 看的。
#
# 上面那个看门狗有个盲区——它是本进程的一个线程，要拿到 GIL 才跑得起来。而实测
# 那次卡死里，60 秒的 asyncio.wait_for 一次都没响过，这说明事件循环当时根本没被
# 调度，也就是卡住的原生调用连 GIL 一起攥着。那种情况下看门狗线程同样醒不来。
# 所以还得有一道在进程外：这行心跳由事件循环自己写，循环一停它就不动了，
# main.js 看见它发霉就直接杀进程重来。
HEARTBEAT_FILE = ROOT / "engine-heartbeat"
HEARTBEAT_S = 5


async def heartbeat():
    while True:
        try:
            HEARTBEAT_FILE.write_text(str(time.time()), encoding="utf-8")
        except Exception:
            LOG.debug("心跳写入失败", exc_info=True)
        await asyncio.sleep(HEARTBEAT_S)


class GpuTicket:
    def __init__(self, scheduler, key, estimate, compatibility):
        self.scheduler = scheduler
        self.key = key
        self.estimate = estimate
        self.compatibility = compatibility
        self.released = False

    async def release(self):
        if self.released:
            return
        self.released = True
        # shield：release() 在 condition 上会挂起，此时再来一次取消
        # （cancel_audio 后紧跟断线）会让 pop 永远执行不到，
        # 票就永久留在 active 里——独占模式下等于整个引擎废掉。
        await asyncio.shield(self.scheduler.release(self.key))

    async def __aenter__(self):
        return self

    async def __aexit__(self, _typ, _value, _tb):
        await self.release()


class GpuScheduler:
    """FIFO admission with live VRAM sampling and balanced, idempotent reservations."""
    # 没有哪个 GPU 阶段该跑这么久。超过就认定持有者已经死了（线程卡在驱动里、
    # 任务被取消时没还票），强制回收——否则独占模式下一张幽灵票会让之后
    # 每一次听写都卡在准入，且跨重连存活，只能重启引擎。
    LEASE_S = 300

    def __init__(self, provider=None):
        self.provider = provider or GPU_PROVIDER
        self.condition = asyncio.Condition()
        self.waiters = deque()
        self.active: dict[tuple[str, str], tuple[int, str, float]] = {}
        self.last_sample: tuple[int, int] | None = None

    @staticmethod
    def reserve_percent() -> int:
        """写死 15%。这个数原来是设置页上的一个输入框，但它调的是显存准入门槛——
        调低了排队变频繁，调高了白白空着一块卡，而正确答案只跟显卡型号有关，
        跟用户的偏好无关。没人有依据去改它，留在界面上只是多一个能填错的格子。"""
        return 15

    def _evict_expired(self):
        now = time.monotonic()
        for key, (_size, _group, started) in list(self.active.items()):
            if now - started > self.LEASE_S:
                LOG.warning("GPU ticket %s/%s exceeded lease — 强制回收", *key)
                self.active.pop(key, None)

    def _compatible(self, compatibility: str) -> bool:
        active_groups = [group for _size, group, _started in self.active.values()]
        if compatibility == "exclusive" or "exclusive" in active_groups:
            return not active_groups
        return True

    async def acquire(self, job_id: str, stage: str, estimate_mb: int,
                      compatibility="shared", on_queued=None) -> GpuTicket:
        key = (job_id, stage)
        # CPU 模式下权重根本不进显存，再按显存排队就是凭空自我阻塞——
        # 尤其显卡被 ComfyUI 占满时，永远等不到"够用"的那一刻。
        if cpu_only():
            async with self.condition:
                self.active[key] = (0, compatibility, time.monotonic())
            return GpuTicket(self, key, 0, compatibility)
        estimate = max(1, int(estimate_mb)) * 1024 * 1024
        waiter = object()
        queued_sent = False
        async with self.condition:
            self.waiters.append(waiter)
            try:
                while True:
                    sample = await self.provider.sample()
                    self.last_sample = sample
                    self._evict_expired()
                    first = self.waiters and self.waiters[0] is waiter
                    active_reserved = sum(size for size, _group, _started in self.active.values())
                    if sample is None:
                        enough = not self.active
                    else:
                        total, free = sample
                        reserve = total * self.reserve_percent() // 100
                        enough = free - reserve - active_reserved >= estimate
                    # 没有任何活动票时必须放行：常驻模型权重不会自己消失，等下去
                    # 没人来 release 唤醒 → 死锁。独占一张票 = 原来的串行行为。
                    if first and (enough or not self.active) and self._compatible(compatibility):
                        self.waiters.popleft()
                        self.active[key] = (estimate, compatibility, time.monotonic())
                        return GpuTicket(self, key, estimate, compatibility)
                    if not queued_sent and on_queued:
                        queued_sent = True
                        status = self.status()

                        async def notify_queued():
                            try:
                                await on_queued(status, estimate_mb)
                            except Exception:
                                LOG.debug(
                                    "queued notification failed for %s/%s",
                                    job_id,
                                    stage,
                                    exc_info=True,
                                )

                        asyncio.create_task(notify_queued())
                    # 等待必须有上限。唤醒只在 release() 时发生，而显存可能被
                    # 第三方进程占着——此时队列里所有人等的是一个不会到来的事件。
                    # 超时后重新采样：显存回来了就走 enough，没回来就走 not self.active
                    # 独占放行（= 原来的串行行为），绝不无限等。
                    try:
                        await asyncio.wait_for(self.condition.wait(), timeout=5)
                    except asyncio.TimeoutError:
                        pass
            except BaseException:
                try:
                    self.waiters.remove(waiter)
                except ValueError:
                    pass
                self.condition.notify_all()
                raise

    async def release(self, key):
        async with self.condition:
            if self.active.pop(key, None) is None:
                return
            self.condition.notify_all()
        # 采样放到锁外：nvidia-smi 回退路径要几百毫秒，占着 condition 采样会让
        # 所有等待者和其它 release 一起排在后面。等待循环本来就会自己重采。
        self.last_sample = await self.provider.sample()
        async with self.condition:
            self.condition.notify_all()

    def status(self) -> dict:
        total, free = self.last_sample or (0, 0)
        return {
            "total_mb": round(total / 1024 / 1024),
            "free_mb": round(free / 1024 / 1024),
            "reserve_percent": self.reserve_percent(),
            "reserved_mb": round(sum(x[0] for x in self.active.values()) / 1024 / 1024),
            "active": len(self.active),
            "queued": len(self.waiters),
            "diagnostics": bool(self.last_sample),
        }


def _model_mb(model_path: Path | None) -> float:
    """权重大小。HF 后端给的是目录，取里面权重文件之和。"""
    if not model_path or not model_path.exists():
        return 0.0
    if model_path.is_file():
        return model_path.stat().st_size / 1024 / 1024
    total = 0
    for pattern in ("*.safetensors", "*.bin"):
        for f in model_path.glob(pattern):
            try:
                total += f.stat().st_size
            except OSError:
                pass
    return total / 1024 / 1024


# 估算的是"这一阶段会新增多少显存"。模型权重在阶段内部才加载（qwen-asr 每次
# 用完就卸载，llama.cpp 换 role 就 close 重开），所以权重本身必须算进增量，
# 不能按"权重常驻、只留 workspace"来估——那样会低估 2.5~4 倍，把本该排队的
# 阶段放进来，externally 占显存时直接退化成共享内存慢上百倍。
def stage_estimate_mb(stage: str, model_path: Path | None = None) -> int:
    weights = _model_mb(model_path)
    if stage == "asr":
        # fp16 权重 ≈ 文件大小，再加处理器/激活的余量。
        return max(900, min(8192, int(weights * 1.15) + 512)) if weights else 900
    if weights:
        # GGUF 权重基本等于文件大小，再加 n_ctx=8192 的 KV 和计算缓冲。
        return max(768, min(8192, int(weights * 1.15) + 1400))
    return 1536


# ---------------------------------------------------------------- ASR

# Qwen3-ASR 的 system 段只认语言全名。正常走 processor 自己的映射表，这张表只是
# 它改名/换实现时的兜底，覆盖设置页语言卡里能勾的那几种就够了。
_QWEN_LANG_NAMES = {"zh": "Chinese", "yue": "Cantonese", "en": "English", "ja": "Japanese",
                    "ko": "Korean", "ru": "Russian", "fr": "French", "de": "German",
                    "es": "Spanish", "pt": "Portuguese", "it": "Italian"}

# Qwen3-ASR 的长 system 上下文会随着音频置信度和内容变化而降低可靠性，可能返回空串，
# 但不存在固定的 550 字硬上限。480 是经过实测的保守默认值；用户可以按词表规模提高预算。
QWEN_CTX_MAX_CHARS = 480
QWEN_CTX_MIN_CHARS = 100
QWEN_CTX_MAX_ALLOWED = 4000


def qwen_ctx_limit(value=None) -> int:
    """把设置值夹到可支持范围；无效值退回保守默认值。"""
    try:
        value = int(value)
    except (TypeError, ValueError, OverflowError):
        return QWEN_CTX_MAX_CHARS
    return max(QWEN_CTX_MIN_CHARS, min(QWEN_CTX_MAX_ALLOWED, value))


def clip_qwen_ctx(ctx: str | None, max_chars=None) -> str | None:
    """按配置预算和分隔符截断上下文，尽量不把词切成半个。"""
    limit = qwen_ctx_limit(max_chars)
    if not ctx or len(ctx) <= limit:
        return ctx
    cut = ctx[:limit]
    for sep in ("; ", ", ", "、", " "):
        i = cut.rfind(sep)
        if i > limit // 2:
            cut = cut[:i]
            break
    LOG.warning("Qwen3-ASR 上下文 %d 字超过本次可靠性预算 %d，截到 %d 字",
                len(ctx), limit, len(cut))
    return cut


class Asr:
    @staticmethod
    def _resolve():
        st = load_settings(); catalog = model_catalog()
        # 配置文件里可能还留着已经删掉的 whisper 之类的 id，认不出来就回落到默认。
        meta = catalog.get(st.get("speechModel")) or catalog[ASR_DEFAULT_ID]
        path = catalog_path(meta)
        if not path.exists(): meta=catalog[ASR_DEFAULT_ID];path=catalog_path(meta)
        return meta, path

    def __init__(self):
        meta, path = self._resolve()
        self.model_id=meta["id"];self.path=path;self.backend=meta.get("backend");self.lock=asyncio.Lock()
        self.model=None;self.processor=None
        # 和 Llm 同理：加载/推理/卸载串在一个线程上，取消后残留的线程不会
        # 和下一次调用抢同一份权重。
        self.pool=ThreadPoolExecutor(max_workers=1, thread_name_prefix="asr")
        # 上一次真正送进模型的 system 文本（已过截断），给流程面板和提示词页看。
        self.last_prompt=""
        LOG.info("ASR configured %s (%s)",self.model_id,self.backend)

    async def reconfigure(self) -> bool:
        """设置页换了语音模型就地换掉。以前这份配置只在 __init__ 读一次，
        换模型得等引擎进程重启才生效——用户看到的就是"换了没反应"。
        返回 True 表示确实换了（调用方好据此提示正在加载）。"""
        meta, path = self._resolve()
        if meta["id"] == self.model_id:
            return False
        async with self.lock:
            old = self.model_id
            self.model_id=meta["id"];self.path=path;self.backend=meta.get("backend")
            self.model=None;self.processor=None
            await self._run(self._cuda_cleanup)
            LOG.info("ASR switched %s → %s (%s)", old, self.model_id, self.backend)
        return True

    def pending_change(self) -> bool:
        return self._resolve()[0]["id"] != self.model_id

    async def preload(self):
        """常驻模式下把权重提前载进显存。

        平时权重是在 GPU 票内部临时加载、用完就卸的，提前占显存只会让真正录音时
        又得先让路。开了「常驻显存」以后这个前提不成立了：权重本来就不卸，提前
        载进去省的就是第一次听写的冷启动。
        """
        if self.model is not None: return
        async with self.lock:
            await self._run(self._ensure_loaded)

    async def _run(self, fn):
        return await asyncio.get_running_loop().run_in_executor(self.pool, fn)

    @staticmethod
    def _cuda_cleanup():
        try:
            import gc, torch
            gc.collect()
            if torch.cuda.is_available(): torch.cuda.empty_cache()
        except Exception:
            pass

    def _ensure_loaded(self):
        if self.model is not None: return
        cpu = cpu_only()
        LOG.info("loading ASR %s (%s) on %s",self.model_id,self.backend,"cpu" if cpu else "cuda")
        # 加载同样会卡死：实测显存满的时候 "loading ASR ... on cuda" 之后就再没有
        # 下文，连 warmup done 都等不到。所以这里也要有闸。
        with gpu_watchdog(f"加载 ASR {self.model_id}", LOAD_WATCH_S):
            import torch
            from transformers import AutoProcessor, AutoModelForMultimodalLM
            self.processor=AutoProcessor.from_pretrained(str(self.path),local_files_only=True)
            self.model=AutoModelForMultimodalLM.from_pretrained(
                str(self.path),
                torch_dtype=torch.float32 if cpu else torch.float16,
                device_map="cpu" if cpu else "cuda",local_files_only=True)

    async def unload(self):
        async with self.lock:
            self.model=None;self.processor=None
            await self._run(self._cuda_cleanup)
            LOG.info("unloaded ASR %s",self.model_id)

    async def transcribe(self, pcm: np.ndarray, initial_prompt: str | None,
                         lang: str | None = None,
                         qwen_ctx_max_chars=None) -> tuple[str, str | None]:
        def run():
            self._ensure_loaded()
            # 闸设在这儿而不是外面的 asyncio 层：真正会卡死的是下面这段原生调用，
            # 而 asyncio 那圈 wait_for 拦不住它（实测一次都没响过）。
            with gpu_watchdog(f"ASR 推理 {self.model_id} ({len(pcm)/SAMPLE_RATE:.1f}s 音频)",
                              max(ASR_WATCH_S, asr_budget(len(pcm) / SAMPLE_RATE))):
                inputs=self._qwen_inputs(pcm,initial_prompt,lang,qwen_ctx_max_chars).to(self.model.device,self.model.dtype)
                max_tokens=max(32,min(256,int(len(pcm)/SAMPLE_RATE*12)+24))
                ids=self.model.generate(**inputs,max_new_tokens=max_tokens,do_sample=False)[:,inputs["input_ids"].shape[1]:]
                text=self.processor.decode(ids,return_format="transcription_only")[0]
                return str(text).strip(),lang
        async with self.lock:
            try:
                return await self._run(run)
            except Exception as e:
                if cpu_only() or not is_gpu_failure(e):
                    raise
                # 显卡在推理中途炸了。降级到内存重跑一次——用户多等几秒，但拿得到
                # 文字。直接把异常抛上去就是药丸上一句"听写失败"，那句话就白说了。
                DEVICE.demote(f"ASR 推理失败：{type(e).__name__}")
                self.model=None;self.processor=None
                await self._run(self._cuda_cleanup)
                return await self._run(run)

    def _qwen_lang_name(self, lang: str | None) -> str | None:
        """'zh' → 'Chinese'。借用 processor 自己的映射表，保证和内置路径一致。"""
        if not lang:
            return None
        try:
            mod = sys.modules[type(self.processor).__module__]
            return mod._prepare_language_inputs(lang, 1)[0]
        except Exception:
            # 私有 helper 改名、或语言码它不认识：退回自带的小表，再不行就当自动检测。
            # 丢一个语言提示，总比让整次听写抛异常强。
            return _QWEN_LANG_NAMES.get(str(lang).split("-")[0].lower())

    def _qwen_inputs(self, pcm: np.ndarray, ctx: str | None, lang: str | None,
                     max_chars=None):
        """Qwen3-ASR 的上下文（词表/历史）只认 system 段。

        原来这里写的是 `apply_transcription_request(..., prompt=initial_prompt)`，而那个函数
        的签名是 `(audio, language=None, **kwargs)`——`prompt` 不是它的参数，多余的 kwargs
        转给 apply_chat_template 后同样不认识，transformers 打一行警告就丢掉了。实测带词表
        和不带词表的 input_ids 一模一样（都是 29 token），所以自定义词表对听写从来没生效过。
        `context` / `system_prompt` / `hotwords` 这几个名字试过，全都无效。

        这里照着 apply_transcription_request 的参考实现手搓 messages：语言名一条 system，
        词表再一条。chat 模板会把所有 system 消息的文本**直接拼接**（中间不加分隔），
        所以换行得自己补，否则会得到「Chinese人名: ...」这种粘在一起的东西。
        """
        name = self._qwen_lang_name(lang)
        ctx = clip_qwen_ctx(ctx, max_chars)
        msgs = []
        if name:
            msgs.append({"role": "system", "content": [{"type": "text", "text": name}]})
        if ctx:
            msgs.append({"role": "system",
                         "content": [{"type": "text", "text": ("\n" if name else "") + ctx}]})
        # 记下**截断之后**真正进模型的那份 system 文本。流程面板以前显示的是
        # 截断前的字符串，和模型实际读到的能差上一千多字——看着以为发出去了，
        # 其实尾巴早被 clip_qwen_ctx 切掉了。
        self.last_prompt = "\n".join(x for x in (name, ctx) if x)
        msgs.append({"role": "user",
                     "content": [{"type": "audio", "audio": pcm.astype(np.float32)}]})
        return self.processor.apply_chat_template([msgs], tokenize=True,
                                                  add_generation_prompt=True, return_dict=True)

# ---------------------------------------------------------------- LLM

class Llm:
    def __init__(self):
        self.lock = asyncio.Lock()
        self.model_path = None
        self.llm = None
        self.fmt = None      # (Jinja2ChatFormatter, eos_token_text)
        # 所有 llama.cpp 调用都串在这一个线程上。asyncio.wait_for 取消时只是
        # 放弃等待，工作线程还在 llama_decode 里跑；此时下一次调用拿到已释放的
        # 锁去 close() 同一个 ctx，就是原生崩溃或卡死在驱动里（连 GIL 一起卡）。
        # 单线程 executor 让加载/推理/释放天然互斥。
        self.pool = ThreadPoolExecutor(max_workers=1, thread_name_prefix="llm")

    async def _run(self, fn):
        return await asyncio.get_running_loop().run_in_executor(self.pool, fn)

    def _formatter(self):
        """自建一份 Jinja2ChatFormatter，好把 enable_thinking 传进模板。
        llama_cpp 自己那份被 to_chat_handler() 包住了，拿不到裸的 render。"""
        if self.fmt is not None:
            return self.fmt
        try:
            from llama_cpp.llama_chat_format import Jinja2ChatFormatter
            tpl = self.llm.metadata.get("tokenizer.chat_template")
            if not tpl: return None
            eos = self.llm._model.token_get_text(self.llm.token_eos())
            bos_id = self.llm.token_bos()
            bos = self.llm._model.token_get_text(bos_id) if bos_id != -1 else ""
            self.fmt = (Jinja2ChatFormatter(template=tpl, eos_token=eos, bos_token=bos), eos)
            return self.fmt
        except Exception:
            LOG.exception("chat template 取不到，退回 create_chat_completion")
            return None

    @staticmethod
    def _load(model_path):
        from llama_cpp import Llama
        cpu = cpu_only()
        LOG.info("loading llm %s on %s", model_path.name, "cpu" if cpu else "gpu")
        # n_gpu_layers=0 → 权重全留在内存里，显存一点都不碰。
        return Llama(model_path=str(model_path), n_ctx=8192,
                     n_gpu_layers=0 if cpu else -1, verbose=False)

    @staticmethod
    def _cleanup():
        try:
            import gc, torch
            gc.collect()
            if torch.cuda.is_available(): torch.cuda.empty_cache()
        except Exception:
            pass

    async def unload(self):
        async with self.lock:
            if self.llm is None:
                return
            # close() 必须和推理走同一个线程，否则可能在别的线程还在解码时
            # 释放同一个 ctx。
            doomed = self.llm
            self.llm = self.fmt = self.model_path = None
            def drop():
                doomed.close()
                self._cleanup()
            await self._run(drop)

    async def chat(self, system: str, user: str, max_tokens: int = 1024, model_path=None,
                   thinking: bool | None = None) -> str:
        """thinking=False → 走 chat template 的 enable_thinking=false：模型开局就拿到一对
        闭合的 <think></think>，压根不会去想。这是**主动关思考**，不是事后截断。
        模板不认这个变量时（Qwen3-4B-Instruct 这种非思考模型）渲染器忽略它，行为不变。"""
        def ensure():
            target = model_path or QWEN_GGUF
            if self.llm is None or target != self.model_path:
                if self.llm is not None: self.llm.close()
                self.llm = None
                self.fmt = None
                self._cleanup()
                self.llm = self._load(target)
                self.model_path = target

        def prompt_of(sys_prompt, think_flag):
            """把 messages 渲染成裸 prompt，好把 enable_thinking 送进 chat template。
            create_chat_completion 不转发未知 kwargs，只有直接调 formatter 才行。"""
            msgs = [{"role": "system", "content": sys_prompt}, {"role": "user", "content": user}]
            f = self._formatter()
            if f is None:
                return None, None, msgs
            fmt, eos = f
            kw = {} if think_flag is None else {"enable_thinking": bool(think_flag)}
            return fmt(messages=msgs, **kw).prompt, eos, msgs

        def once(sys_prompt, cap, think_flag=None):
            p, eos, msgs = prompt_of(sys_prompt, think_flag)
            if p is None:      # 没有 chat template 的模型：退回原路径
                r = self.llm.create_chat_completion(messages=msgs, max_tokens=cap, temperature=0.3)
                return r["choices"][0]["message"]["content"].strip()
            r = self.llm.create_completion(p, max_tokens=cap, temperature=0.3, stop=[eos])
            return r["choices"][0]["text"].strip()

        def run():
            ensure()
            # 主动关思考：模板拿到 enable_thinking=false 会直接写好空的 <think></think>
            if thinking is False:
                return once(NO_THINK_SYSTEM, max_tokens, False)
            return once(system, max_tokens, thinking)

        async with self.lock:
            try:
                return await self._run(run)
            except Exception as e:
                if cpu_only() or not is_gpu_failure(e):
                    raise
                # 学词模型在显卡上炸了。和 ASR 同理：降级重跑一次，别让这次判定
                # 白白失败。
                # llama.cpp 炸掉之后那个 ctx 不能再碰，必须先彻底丢干净再重开，
                # 否则下一次 create_completion 就是原生崩溃。
                DEVICE.demote(f"学词模型失败：{type(e).__name__}")

                def drop():
                    doomed, self.llm = self.llm, None
                    self.fmt = self.model_path = None
                    if doomed is not None:
                        try:
                            doomed.close()
                        except Exception:
                            LOG.debug("丢弃炸掉的 llama ctx 时又抛了一次", exc_info=True)
                    self._cleanup()

                await self._run(drop)
                return await self._run(run)

# 设置页「提示词」的默认值。正文在 prompts.json 里，engine 和 settings.html 共读一份。
# 以前两边各存一份副本，改了引擎那份、设置页「恢复默认」拿到的还是旧的——同一个坑踩过两次。
_PROMPT_FILE = json.loads(PROMPTS_PATH.read_text(encoding="utf-8"))
DEFAULT_PROMPTS = _PROMPT_FILE["defaults"]
# 被取代的旧默认。保存值与其中一条逐字相同 = 用户从没自定义过，可以安全升级。
LEGACY_PROMPTS = _PROMPT_FILE.get("legacy", {})

# ASR 在静音/低信噪段上会吐训练集里的字幕尾巴。这些句子用户从没说过，
# 但读起来完全通顺，精修模型不但不会删，还会帮着润色成正文。只能在进 LLM 前挡掉。
HALLUCINATION_LINES = (
    "感谢观看", "谢谢观看", "感谢收看", "谢谢收看", "字幕由Amara.org社区提供",
    "请不吝点赞", "点赞订阅", "订阅我的频道", "明镜与点点栏目",
    "下次再见", "我们下期再见", "Thanks for watching", "Thank you for watching",
    "Subscribe to my channel", "Please subscribe",
)


def _dehallucinate(text: str) -> str:
    """清掉 ASR 的静音幻觉：成片的重复符号、整句复读、训练集字幕尾巴。"""
    if not text:
        return ""
    # 装饰性填充符成片出现（'．﹏﹏﹏﹏…'）纯粹是幻觉，整段删掉；
    # 真标点复读（'。。。'、'！！！'）只压成一个，别把语气也吃了。
    text = re.sub(r"[﹏～~_─—=+*#·・]{2,}", "", text)
    text = re.sub(r"([^\w\s一-鿿])\1{2,}", r"\1", text)
    text = re.sub(r"[\s　]+", " ", text).strip()
    # 整句被复读：'好了我再试一试？好了我再试一试？' → 只留一份。
    # 只处理"前一半和后一半完全相同"这种规整重复，别去猜用户真正的重复表达。
    half = len(text) // 2
    if half >= 4 and text[:half].strip(" ,.，。") == text[half:].strip(" ,.，。"):
        text = text[:half].strip()
    for line in HALLUCINATION_LINES:
        # 只在整段就是它、或它单独占一句时才删，避免误伤正文里真的提到这几个字
        if text.strip(" ,.!?，。！？") == line:
            return ""
        text = re.sub(r"(?:(?<=[。．.!?！？])|^)\s*" + re.escape(line) + r"\s*[。．.!?！？]?",
                      "", text).strip()
    return text.strip()


def _strip_think(text: str) -> str:
    """Qwen3 may emit <think>...</think> blocks even for instruct models; drop them.
    max_tokens 截断时收不到 </think>，剩下的整段都是思维链——一并丢掉，绝不外泄。"""
    text = re.sub(r"<think>.*?</think>", "", text or "", flags=re.S)
    text = re.sub(r"<think>.*\Z", "", text, flags=re.S)      # 未闭合：截断在思考中
    text = re.sub(r"\A.*?</think>", "", text, flags=re.S)    # 只有结束标签：开头被吃掉
    return text.strip()


# ---------------------------------------------------------------- VAD

class Vad:
    """Silero VAD if the ONNX file exists, else RMS-energy gate."""
    def __init__(self):
        self.sess = None
        if VAD_ONNX.exists() and VAD_ONNX.stat().st_size > 100_000:
            try:
                import onnxruntime as ort
                opts = ort.SessionOptions()
                opts.inter_op_num_threads = opts.intra_op_num_threads = 1
                self.sess = ort.InferenceSession(str(VAD_ONNX), opts, providers=["CPUExecutionProvider"])
                self.reset()
            except Exception:
                LOG.exception("silero load failed, using energy VAD")
                self.sess = None
        else:
            LOG.warning("no silero model, using energy VAD")

    def reset(self):
        if self.sess:
            self.state = np.zeros((2, 1, 128), np.float32)

    def prob(self, frame512: np.ndarray) -> float:
        if not self.sess:
            rms = float(np.sqrt(np.mean(frame512 ** 2)))
            return min(1.0, rms / 0.02)  # crude: 0.02 RMS ≈ speech
        out, self.state = self.sess.run(
            None, {"input": frame512[None, :].astype(np.float32),
                   "state": self.state, "sr": np.array(SAMPLE_RATE, np.int64)})
        return float(out.reshape(-1)[0])

# ---------------------------------------------------------------- session

class Session:
    def __init__(self, ws, audio_id: str, asr: Asr, llm: Llm, scheduler: GpuScheduler,
                 send_json=None):
        self.ws = ws
        self.audio_id = audio_id
        self.asr = asr
        self.llm = llm
        self.scheduler = scheduler
        self.send_json = send_json or (lambda data: self.ws.send_str(json.dumps(data)))
        self.mode = "transcript"
        self.params: dict = {}
        self.chunks: list[bytes] = []          # 处理过、未限幅的 float32
        self.sample_rate = SAMPLE_RATE
        self.meta: dict = {}
        self.vad = Vad()
        self.qwen_ctx_max_chars = QWEN_CTX_MAX_CHARS
        self.audio_processor = AudioProcessor()
        self.trailing_silence_s = 0.0
        self.voiced_s = 0.0                     # 本段录音累计的人声时长
        self.total_s = 0.0
        self.committed_text = ""
        self.finalized = False
        self.pending: dict[str, dict] = {}     # audio_id -> last refine result (for http fallback)
        self.auto_detect = True                 # 自动检测 vs 仅允许列表内语言
        self.allowed_langs: set[str] = set()    # autoDetect=false 时允许的语言码集合
        self.history: list[str] = []
        self.corrections: list[dict] = []
        self.focused_text = ""
        self.target_app = ""
        self.raw_text = ""
        self.duration = 0.0

    # ---- incoming
    async def on_start(self, msg: dict):
        self.meta = msg.get("audio_metadata") or {}
        self.mode = (msg.get("mode") or "transcript")
        self.sample_rate = int(self.meta.get("audio_sample_rate") or msg.get("audio_sample_rate") or SAMPLE_RATE)
        self.history = [str(x) for x in (msg.get("history") or [])][-20:]
        self.corrections = [x for x in (msg.get("corrections") or []) if isinstance(x, dict)][:30]
        self.focused_text = str(msg.get("focused_text") or "")[-500:]
        self.target_app = str(msg.get("target_app") or "")[:120]
        st = load_settings()
        # 录音期间冻结音频与 ASR 设置，避免一段录音前后使用两套处理参数。
        self.qwen_ctx_max_chars = qwen_ctx_limit(st.get("qwenCtxMaxChars"))
        self.audio_processor = AudioProcessor(
            st.get("micGainDb", 0), st.get("noiseReductionEnabled", False)
        )
        # autoDetect=true → 自动检测（不传 language）；false → 仅允许列表内语言
        self.auto_detect = bool(st.get("autoDetect", True))
        self.allowed_langs = {("zh-Hans" if str(x) == "zh" else str(x)) for x in (st.get("languages") or []) if x}
        # 设置页换过语音模型就在这儿就地换掉。换模型和首次加载都要读几秒权重，
        # 先把 loading 发出去，用户才知道是在装模型而不是卡死了。
        if self.asr.pending_change():
            await self.emit_pipeline("loading", status="running", model=self.asr.model_id)
            try:
                await self.asr.reconfigure()
            finally:
                await self.emit_pipeline("loading", status="completed", model=self.asr.model_id)
        self.voiced_s = 0.0
        LOG.info("start %s mode=%s langAuto=%s allowed=%s meta=%s", self.audio_id[:8], self.mode,
                 self.auto_detect, sorted(self.allowed_langs),
                 {k: self.meta.get(k) for k in ("audio_format", "audio_sample_rate", "audio_channels")})

    async def on_mode_config(self, msg: dict):
        self.mode = msg.get("mode") or self.mode
        self.params = msg.get("parameters") or self.params
        LOG.info("mode_config %s %s", self.mode, self.params)

    def allowed_bare(self) -> set[str]:
        return {x.split("-")[0] for x in self.allowed_langs if x}

    def asr_lang(self) -> str | None:
        """ASR language 参数：自动检测→None；仅允许列表→若恰好一种就锁死，
        多语言则 None（交给自动检测）。"""
        if self.auto_detect:
            return None
        allowed = [x for x in self.allowed_langs if x]
        return allowed[0].split("-")[0] if len(allowed) == 1 else None

    def force_traditional(self) -> bool:
        """自动检测开启、且允许列表只含繁中不含简中时，输出用 OpenCC 转繁体。"""
        return self.auto_detect and "zh-Hant" in self.allowed_langs and "zh-Hans" not in self.allowed_langs

    async def on_chunk(self, buf: bytes):
        try:
            pcm = frame_to_pcm16(buf, self.meta)
        except Exception:
            LOG.exception("chunk decode failed")
            return
        raw = np.frombuffer(pcm, np.int16).astype(np.float32) / 32768.0
        x = self.audio_processor.process(raw)
        # 存储、VAD 和 ASR 必须听同一份处理结果。
        #
        # 存 float32 而不是 int16：process() 出来的电平可能超过 ±1（增益已经加了，
        # 限幅要等整段录完才做，见 normalize），存成 int16 就等于在这儿先硬夹一刀，
        # 比原来那个软限幅还难听，而且夹完就再也退不回去了。多占的内存是每秒
        # 32 KB，一分钟的录音也就 2 MB。
        self.chunks.append(x.astype(np.float32, copy=False).tobytes())
        # VAD on 512-sample frames of the new chunk
        #
        # 喂给 VAD 的仍然是限幅过的那一份，和改动之前逐字节相同。VAD 是个对电平
        # 敏感的判别器，而存下来的这一份现在可能超过 ±1（热麦 ×3.162 能到 3.16），
        # 直接丢进去等于换了一组它没见过的输入。整段的归一化又要等录完才做，这里
        # 拿不到。软限幅是唯一能就地复现原输入的东西。
        v = self.audio_processor._soft_limit(x)
        for off in range(0, len(v) - 511, 512):
            p = self.vad.prob(v[off:off + 512])
            if p > 0.5:
                self.trailing_silence_s = 0.0
                self.voiced_s += 512 / SAMPLE_RATE
            else:
                self.trailing_silence_s += 512 / SAMPLE_RATE
        self.total_s += len(x) / SAMPLE_RATE

    async def on_end(self):
        await self.finalize()

    # ---- outgoing
    def audio_f32(self) -> np.ndarray:
        """录下来的整段，**还没限幅**。要喂给模型的话过一遍 normalize。"""
        if not self.chunks:
            return np.zeros(0, np.float32)
        return np.frombuffer(b"".join(self.chunks), np.float32)

    async def emit_pipeline(self, stage: str, **data):
        # 纯遥测：设置页看不到进度是小事，卡死整条流水线是大事。WS 背压时
        # send_lock 会一直握着，不设上限就会把持票的阶段永久挂住。
        try:
            await asyncio.wait_for(self.send_json({
                "type": "pipeline_update", "audio_id": self.audio_id,
                "stage": stage, **data}), timeout=5)
        except asyncio.CancelledError:
            raise
        except Exception:
            LOG.debug("pipeline update %s dropped", stage, exc_info=True)

    async def gpu_ticket(self, stage: str, estimate_mb: int, compatibility="shared"):
        async def queued(status, estimate):
            await self.emit_pipeline(stage, status="queued", estimate_mb=estimate, gpu=status)
        ticket = await self.scheduler.acquire(self.audio_id, stage, estimate_mb, compatibility, queued)
        # 拿到票之后任何异常都必须还票，否则 active 里留一张幽灵票，
        # 后续所有阶段都等一个永远不会到来的 release。
        try:
            await self.emit_pipeline(stage, status="admitted", estimate_mb=estimate_mb,
                                     gpu=self.scheduler.status())
        except BaseException:
            await ticket.release()
            raise
        return ticket

    async def finalize(self):
        if self.finalized:
            return
        self.finalized = True
        pcm = self.audio_processor.normalize(self.audio_f32())
        dur = len(pcm) / SAMPLE_RATE
        LOG.info("finalize %s: %.1fs audio", self.audio_id[:8], dur)
        try:
            if dur < 0.35:
                await self.send_refined("")
                return
            t0 = time.monotonic()
            # ASR/LLM 任一环节抛异常都必须兜住——否则协程死在锁里，refine_completed
            # 永远发不出去，客户端药丸会一直停在"转写中…"。
            st = load_settings()
            wprompt = asr_prompt(st, self.focused_text)
            # 常驻模式下两边都不卸：让路的前提是显存不够同时装下，16 GB 卡上装得下。
            # ——但那句"装得下"只在这张卡归自己时成立。ComfyUI 一起来就不成立了，
            # 而显存耗尽在 Windows 上不报错、只是卡死（见 ASR_FREE_FLOOR_MB 那段），
            # 所以这里先看一眼实际余量：不够就本轮放弃常驻，把 LLM 让出去。
            # 宁可多花一次冷加载的两秒，也不要挂死等人来杀进程。
            resident = asr_resident(st)
            if resident:
                free = free_vram_mb()
                if free is not None and free < ASR_FREE_FLOOR_MB:
                    LOG.warning("显存只剩 %d MB（低于 %d MB），本轮放弃常驻：先卸掉学词模型再听写",
                                free, ASR_FREE_FLOOR_MB)
                    resident = False
            if not resident:
                await self.llm.unload()
                # 让完路再看一眼。还是不够就干脆别开始：这时候硬上等于卡死一分半，
                # 再被看门狗连进程一起收走，比当场说一句"显存不够"糟得多。
                free = free_vram_mb()
                if free is not None and free < ASR_FREE_FLOOR_MB:
                    # 以前这里直接报错收工，用户看到的就是"听写失败"，说的话白说了。
                    # 但显存不够并不代表这次听写做不了，只是不能在显卡上做——
                    # 转内存慢几秒，总比把话丢了强。
                    if DEVICE.demote(f"听写前空闲显存只剩 {free} MB"):
                        LOG.warning("卸掉精修模型后仍只剩 %d MB（需要 %d MB）——本次转用内存跑",
                                    free, ASR_FREE_FLOOR_MB)
                        # 必须把显卡上那份权重丢掉。_ensure_loaded 看见 model 还在
                        # 就直接返回，档位改了也不会重载——那就还在显卡上跑，
                        # 等于这次降级什么也没做。
                        await self.asr.unload()
                    else:
                        # 用户关了自动切换。尊重他：老实报错，别偷偷改设备。
                        LOG.error("显存不足，放弃本次听写：卸掉精修模型后仍只剩 %d MB（需要 %d MB）",
                                  free, ASR_FREE_FLOOR_MB)
                        await asyncio.wait_for(self.send_json({
                            "type": "error", "audio_id": self.audio_id,
                            "message": f"显存不足（只剩 {free} MB），先关掉占显卡的程序再试"}), timeout=10)
                        return
            # Qwen3-ASR 独占显存，整条流水线同一时刻只许一个阶段上卡。
            async with await self.gpu_ticket("asr", stage_estimate_mb("asr", self.asr.path), "exclusive"):
                text, lang = await asyncio.wait_for(
                    self.asr.transcribe(pcm, initial_prompt=wprompt,
                                        lang=self.asr_lang(),
                                        qwen_ctx_max_chars=self.qwen_ctx_max_chars),
                    timeout=asr_budget(dur))
            asr_ms = (time.monotonic() - t0) * 1000
            # 用上面那个 resident（已经把显存余量算进去了），不是重读设置：
            # 前面因为显存紧张才卸的 LLM，这里要是又按设置判成"常驻"，ASR 就会留在
            # 显存里，下一轮两个模型照样挤在一起，等于白让了一次路。
            if not resident:
                await self.asr.unload()
            # 幻觉在这儿就得挡掉：它读起来完全通顺，放进精修反而会被润色成正文。
            raw_asr = text
            text = _dehallucinate(text)
            if text:
                LOG.info("asr %.0fms lang=%s text=%r", asr_ms, lang, text[:120])
            else:
                # 空结果必须分得清是哪一种，两者根因完全不同：模型什么都没吐＝音频
                # 轻到被当成静音跳过（查电平和麦克风增益）；吐了字幕尾巴被整段清掉
                # ＝模型在硬猜（查的是识别本身）。合成一句 text='' 就是让前者伪装成
                # 后者。电平也一并记下：以前只有前端记，engine 侧一无所知，两边对
                # 不上号，一条 30.5s 的空结果查了两轮还是停在猜。
                peak = float(np.abs(pcm).max()) if len(pcm) else 0.0
                rms = float(np.sqrt(np.mean(pcm.astype(np.float64) ** 2))) if len(pcm) else 0.0
                LOG.info("asr %.0fms lang=%s 空结果（%s）原始=%r peak=%.4f rms=%.4f",
                         asr_ms, lang,
                         "模型没吐字" if not raw_asr.strip() else "幻觉被整段清掉",
                         raw_asr[:120], peak, rms)
            if self.force_traditional():
                try:
                    import opencc
                    text = opencc.OpenCC("s2t").convert(text)
                    LOG.info("converted to traditional: %r", text[:120])
                except Exception:
                    LOG.exception("s2t convert failed")
            # 念一遍输入框里已有的字当引子，粘回去就成了"我想把我想把…"。
            # 精确重叠用代码删掉，比让模型去判可靠得多。
            deduped = strip_focused_echo(self.focused_text, text)
            if deduped != text:
                LOG.info("去掉与输入框重复的开头：%r -> %r", text[:40], deduped[:40])
                text = deduped
            self.raw_text = text
            self.duration = dur
            await self.emit_pipeline("asr", status="completed", model=self.asr.model_id,
                                     raw_text=text, language=lang, elapsed_ms=round(asr_ms),
                                     prompt=self.asr.last_prompt or wprompt or "(no initial prompt)")
            # 精修链路整条删掉了：ASR 吐出来的就是最终文本，不再有第二个模型过一手。
            # 所以这里不再记第二条 asr 日志——它和上面那条一字不差。繁体转换和
            # 去输入框回声各自已经有自己的日志行，真被改过一眼就看得见。
            refined = text
            await self.emit_pipeline("completed", status="completed", route="raw",
                                     refined_text=refined, elapsed_ms=round(asr_ms))
        except asyncio.CancelledError:
            # CancelledError 是 BaseException，下面那个 except Exception 接不住它。
            # 客户端超时后会发 cancel_audio，服务端 task.cancel() 就走到这儿——
            # 以前这里什么都不记，于是"听写超时"在 engine.log 里连一行痕迹都没有，
            # 完全看不出是被掐的还是引擎自己死了。原样 re-raise，只补一条日志。
            LOG.warning("finalize %s 被取消（客户端 cancel_audio 或断开），%.1fs 音频作废",
                        self.audio_id[:8], dur)
            raise
        except Exception as exc:
            LOG.exception("finalize %s failed", self.audio_id[:8])
            try:
                await self.emit_pipeline("error", status="error", error=str(exc)[:300])
            except Exception:
                pass
            refined = ""
        # 这几行必须在 try 之后仍然跑到，且自己不能抛——refine_completed 发不出去
        # 客户端就永远停在思考中。mode 不是字符串时 .lower() 会抛，兜住。
        try:
            if str(self.mode).lower() == "transcript":
                self.committed_text = (self.committed_text + " " + refined).strip()
            self.pending[self.audio_id] = {"refined_text": refined, "delivery": "single"}
        except Exception:
            LOG.exception("commit bookkeeping failed")
        await self.send_refined(refined)
        self.chunks.clear()
        self.vad.reset()
        self.total_s = 0.0

    async def send_refined(self, refined: str):
        # 唯一一条真正重要的消息，反而不能无限等 WS 背压。
        try:
            await asyncio.wait_for(self.send_json({
                "type": "refine_completed", "audio_id": self.audio_id,
                "raw_text": self.raw_text, "duration": self.duration,
                "refined_text": refined, "delivery": "single"}), timeout=10)
        except asyncio.CancelledError:
            raise
        except Exception:
            LOG.warning("refine_completed 发送失败 %s", self.audio_id[:8], exc_info=True)

# ---------------------------------------------------------------- ws server

async def review_learning(scheduler, llm, st, data, wrong, right, send_json):
    """自学习判定。全程自己兜底：无论抢票、推理还是解析出错，
    都要回一条 learning_reviewed，且异常绝不能冒到接收循环。"""
    verdict = {"add": False}
    try:
        model = learn_model_path(st)
        prompt = build_learn_prompt(st, wrong, right)
        learning_id = f"learning:{data.get('request_id') or uuid.uuid4().hex}"
        ticket = await scheduler.acquire(learning_id, "learning", stage_estimate_mb("llm", model))
        async with ticket:
            raw = await asyncio.wait_for(
                llm.chat("You are a strict vocabulary curator. /no_think", prompt,
                         max_tokens=120, model_path=model, thinking=False),
                timeout=60)
        match = re.search(r"\{.*\}", _strip_think(raw), re.S)
        if match:
            verdict = json.loads(match.group(0))
    except asyncio.CancelledError:
        raise
    except Exception:
        LOG.warning("learning review failed", exc_info=True)
    term = str(verdict.get("term") or "").strip()[:60]
    tag = str(verdict.get("tag") or "").strip()[:40]
    # 成功也要记一笔：之前只在失败时写日志，学习悄悄停了好几天都看不出来。
    LOG.info("learn %r -> %r add=%s tag=%r", right[:60], term, bool(verdict.get("add") and term), tag)
    try:
        await asyncio.wait_for(send_json({
            "type":"learning_reviewed", "request_id":data.get("request_id"),
            "add":bool(verdict.get("add") and term), "term":term, "tag":tag,
            "history_id":data.get("history_id"), "wrong":wrong, "right":right}), timeout=10)
    except asyncio.CancelledError:
        raise
    except Exception:
        LOG.debug("learning_reviewed 发送失败", exc_info=True)


async def handle(ws, asr, llm, scheduler):
    remote = ws.remote_address
    LOG.info("client connected %s", remote)
    sessions: dict[str, Session] = {}
    tasks: dict[str, asyncio.Task] = {}
    learn_tasks: set[asyncio.Task] = set()
    send_lock = asyncio.Lock()

    async def send_json(data):
        async with send_lock:
            await ws.send_str(json.dumps(data))

    async def protocol_error(audio_id, message):
        await send_json({"type": "protocol_error", "audio_id": audio_id, "error": message})

    def finalize_done(audio_id, task):
        tasks.pop(audio_id, None)
        sessions.pop(audio_id, None)
        if task.cancelled():
            return
        try:
            task.result()
        except Exception:
            LOG.exception("finalize task %s failed", audio_id[:8])

    try:
        async for msg in ws:
            if isinstance(msg, (bytes, bytearray)):
                try:
                    aid, payload = decode_audio_frame(bytes(msg))
                except Exception as exc:
                    await protocol_error(None, str(exc))
                    continue
                if aid is None:
                    active = [sid for sid in sessions if sid not in tasks]
                    if len(active) != 1:
                        await protocol_error(None, "untagged audio is ambiguous")
                        continue
                    aid = active[0]
                session = sessions.get(aid)
                if not session or aid in tasks:
                    await protocol_error(aid, "audio session is not active")
                    continue
                await session.on_chunk(payload)
                continue
            try:
                data = json.loads(msg)
            except Exception:
                await protocol_error(None, "invalid JSON")
                continue
            t = data.get("type")
            aid = data.get("audio_id") or data.get("audioId")
            if t == "start_audio":
                sid = aid or uuid.uuid4().hex
                if sid in sessions:
                    await protocol_error(sid, "duplicate audio_id")
                    continue
                s = Session(ws, sid, asr, llm, scheduler, send_json)
                sessions[sid] = s
                await s.on_start(data)
                await send_json({"type": "session_started", "audio_id": sid})
            elif t == "set_mode_config" and aid in sessions:
                await sessions[aid].on_mode_config(data)
            elif t in ("replace_audio_context", "set_selected_text") and aid in sessions:
                value = data.get("text") or data.get("selected_text") or data.get("audio_context") or ""
                sessions[aid].focused_text = str(value)[-500:]
            elif t == "end_audio":
                if aid not in sessions or aid in tasks:
                    await protocol_error(aid, "unknown or finalized audio_id")
                    continue
                task = asyncio.create_task(sessions[aid].on_end(), name=f"finalize:{aid}")
                tasks[aid] = task
                task.add_done_callback(lambda done, sid=aid: finalize_done(sid, done))
            elif t == "cancel_audio":
                task = tasks.get(aid)
                if task:
                    task.cancel()
                else:
                    sessions.pop(aid, None)
                await send_json({"type": "audio_cancelled", "audio_id": aid})
            elif t == "review_learning":
                wrong, right = str(data.get("wrong") or "")[:300], str(data.get("right") or "")[:300]
                st = load_settings()
                if not st.get("learnEnabled", True):
                    await send_json({"type":"learning_reviewed","request_id":data.get("request_id"),
                                     "add":False,"term":"","tag":"", "history_id":data.get("history_id"),
                                     "wrong":wrong,"right":right})
                    continue
                # 学习判定必须离开接收循环：它要抢票 + 冷加载一个 GGUF，
                # 在循环里 await 就等于这段时间收不到 end_audio/cancel_audio，
                # 而且它抛异常会顺着 async for 冒到 finally，把所有在跑的听写一起取消。
                learn_task = asyncio.create_task(
                    review_learning(scheduler, llm, st, data, wrong, right, send_json),
                    name="review_learning")
                learn_tasks.add(learn_task)
                learn_task.add_done_callback(learn_tasks.discard)
            elif t == "ping":
                # device 和 gpu 分开放：gpu 是调度器的余量账，device 是模型此刻
                # 在哪。托盘只认 device，设置页的 GPU 状态只认 gpu，别互相拖累。
                await send_json({"type": "pong", "gpu": scheduler.status(),
                                 "device": await device_report(asr),
                                 "encoders": await GPU_PROVIDER.encoder_pids()})
            else:
                LOG.debug("ignored msg %s", t)
    except Exception:
        LOG.exception("client error")
    finally:
        pending = list(tasks.values()) + list(learn_tasks)
        for task in pending:
            task.cancel()
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)
        sessions.clear()
        LOG.info("client disconnected %s", remote)


def patch_ws(ws):
    """websockets>=12 send() rejects str? no—send(str) is fine; keep a helper anyway."""
    ws.send_str = ws.send
    return ws


async def warmup(asr: "Asr", llm: "Llm"):
    """把模型提前载进显存，第一次听写就不用等冷启动。开机自启时尤其明显。

    以前这里只预热 llama.cpp 那一侧：transformers-qwen-asr 走独占，用完就卸，
    预热它等于占着显存不放，真正录音时还得先让路，白搭。开了「常驻显存」以后
    权重本来就不卸了，这个理由不再成立，两边一起预热。
    """
    st = load_settings()
    # 只预热 ASR。剩下那个语言模型只为「从修改中学词」活着，是用户点一下才跑的
    # 一次性活——llama.cpp 预热完不卸权重，为它占住几个 GB 显存纯亏，实测就是它
    # 把 ASR 的常驻名额挤没了（日志里那句"显存只剩 5976 MB…跳过 ASR 预热"）。
    if asr_resident(st):
        # 显存紧张时别抢着把 ASR 也常驻进去。两个模型挤满一张卡之后，第一次听写
        # 就会卡死在 attention 的 prefill 里，而且不报错（见 ASR_FREE_FLOOR_MB）。
        # 这时候退回"用完就卸"：每次多等约两秒冷加载，但不会挂死。
        need = stage_estimate_mb("asr", asr.path) + ASR_FREE_FLOOR_MB
        free = free_vram_mb()
        if free is not None and free < need:
            LOG.warning("显存只剩 %d MB（常驻需要 %d MB），跳过 ASR 预热，改成用完就卸", free, need)
        else:
            try:
                await asr.preload()
                LOG.info("warmup done (asr %s 常驻显存)", asr.model_id)
            except Exception:
                LOG.warning("ASR 预热失败 — 首次听写要等冷加载", exc_info=True)


def resident_need_mb(st: dict, asr: "Asr") -> int:
    """把当前开着的模型都搬进显存，需要多少空闲显存。

    现在常驻的只有语音模型一个。学词那个语言模型不预热也不常驻（用完就卸），
    不记进门槛——把一个不会进显存的权重按 ~6.9 GB 算进来，在 16 GB 卡上会把门槛
    顶到 14 GB，等于永远升不了舱：降舱只要空闲跌破 1400 MB，升舱却要整张卡空出
    86%，中间那道坎跨不过去。
    最后加一段余量，理由见 GPU_PROMOTE_HEADROOM_MB。
    """
    return stage_estimate_mb("asr", asr.path) + GPU_PROMOTE_HEADROOM_MB


# 托盘那一行要显示"此刻"的显存余量，而它每几秒问一次、还可能有两个窗口同时问。
# 给个很短的缓存：够新鲜，又不至于把 NVML 敲成筛子。
_VRAM_VIEW: dict = {"at": 0.0, "data": {}}
_VRAM_VIEW_TTL_S = 2.0
_VRAM_VIEW_LOCK = asyncio.Lock()


async def device_report(asr: "Asr") -> dict:
    """托盘要的一整份：档位 + 此刻空闲显存 + 搬回显卡的门槛。

    余量必须是现读的。之前托盘显示的是降舱那一刻写进 `why` 的那个数字，然后
    一直挂着不动——用户对着任务管理器上明明白白的 2.6/16.0 GB，看见托盘说
    "空闲显存只剩 895 MB"，只会认为这软件在瞎报。而那个数字确实是三小时前的。

    门槛也得和 device_tick 算的是同一个 resident_need_mb，否则托盘说"够了"而
    引擎不动，比干脆不显示更糟。
    """
    snap = DEVICE.snapshot()
    async with _VRAM_VIEW_LOCK:
        now = time.monotonic()
        if now - _VRAM_VIEW["at"] >= _VRAM_VIEW_TTL_S:
            sample = await GPU_PROVIDER.sample()
            # 探不到 N 卡就什么都不加，让托盘少显示一行，而不是显示个 0。
            _VRAM_VIEW["data"] = {} if sample is None else {
                "freeMb": sample[1] // 1024 // 1024,
                "needMb": resident_need_mb(load_settings(), asr)}
            _VRAM_VIEW["at"] = now
        snap.update(_VRAM_VIEW["data"])
    return snap


async def reseat_models(asr: "Asr", llm: "Llm"):
    """换档之后把已经加载的权重丢掉，让下一次加载落到新设备上。

    两个模型类都是"加载那一刻读一次 cpu_only()"，所以光改档位不够：不卸掉的话
    权重还在原来的设备上待着，档位改了却什么都没发生。
    """
    for what, fn in (("LLM", llm.unload), ("ASR", asr.unload)):
        try:
            await fn()
        except Exception:
            # 卸载失败不能中断换档：另一个还得接着卸，否则一半在显存一半在内存。
            LOG.debug("换档时卸载 %s 失败", what, exc_info=True)


async def device_tick(asr: "Asr", llm: "Llm", scheduler: "GpuScheduler") -> str | None:
    """盯一次显存，该换档就换。返回 'promote'/'demote'/None。"""
    if not DEVICE.auto():
        return None
    # 手上有活就别动。换档要卸载再重载权重，正在听写的那一轮会当场崩掉。
    if scheduler.active or scheduler.waiters:
        return None
    # 这里故意直接采样，不走 free_vram_mb()：那个函数在 CPU 模式下返回 None，
    # 对它原本的调用方是对的（权重不进显存，余量确实无关）。可我们要判断的恰恰是
    # "还在内存里跑的时候，显卡是不是已经空出来了"——用它就永远等不到升舱那一刻。
    sample = await GPU_PROVIDER.sample()
    if sample is None:
        return None          # 没有 N 卡，或者探测不到：本来就该一直待在内存里
    free_mb = sample[1] // 1024 // 1024
    if DEVICE.cpu_only():
        need = resident_need_mb(load_settings(), asr)
        if not DEVICE.vote(free_mb >= need) or not DEVICE.promote(
                f"空闲 {free_mb} MB ≥ 需要 {need} MB"):
            return None
        await reseat_models(asr, llm)
        # 顺手预热。不然"搬回显卡"要等用户下次开口才真的发生，而那一次就得当场
        # 等冷加载——省下的时间又还回去了，用户还以为是自己那句话特别慢。
        await warmup(asr, llm)
        return "promote"
    # 已经在显卡上跑。掉到地板线以下就主动撤，别等它卡死：WDDM 在显存耗尽时
    # 不报错、只把显存往内存里挪，硬撑下去就是慢到没有尽头，然后被看门狗连
    # 进程一起收走（见 ASR_FREE_FLOOR_MB）。
    if free_mb < ASR_FREE_FLOOR_MB and DEVICE.demote(f"空闲显存只剩 {free_mb} MB"):
        await reseat_models(asr, llm)
        return "demote"
    return None


async def device_monitor(asr: "Asr", llm: "Llm", scheduler: "GpuScheduler"):
    """后台盯着显存：够了就把模型搬回显卡，不够就搬回内存。全程静默，只写日志。"""
    while True:
        await asyncio.sleep(GPU_MONITOR_S)
        try:
            await device_tick(asr, llm, scheduler)
        except Exception:
            # 监测自己出错绝不能连累听写——它只是个锦上添花的后台任务。
            # 取消是 CancelledError（BaseException），不会被这里吞掉，正好。
            LOG.debug("显存监测这一轮出错", exc_info=True)


async def main():
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s",
                        handlers=[logging.StreamHandler(sys.stdout),
                                  logging.FileHandler(ROOT / "engine.log", encoding="utf-8")])
    for p in (ASR_DIR / "model.safetensors", QWEN_GGUF):
        if not p.exists():
            LOG.error("missing model: %s", p); sys.exit(1)
    # 日志配好之后马上起看门狗：加载权重本身也会卡死，那时候连服务都还没监听。
    start_watchdog()
    # 上一条命是被 GPU 卡死杀掉的话，这次先用内存起步，别一头撞回同一堵墙。
    # 必须排在 Asr() 前面：档位是在加载权重那一刻读的，读完就定死了。
    DEVICE.adopt_marker()
    asr, llm = Asr(), Llm()
    scheduler = GpuScheduler()
    import websockets
    async with websockets.serve(
            lambda ws: handle(patch_ws(ws), asr, llm, scheduler), WS_HOST, WS_PORT,
            max_size=64 * 1024 * 1024, ping_interval=20, ping_timeout=60):
        LOG.info("listening ws://%s:%d/ws/rt_voice_flow", WS_HOST, WS_PORT)
        # 心跳要先于预热起来：加载权重就可能卡死，那时候 main.js 得能看出来。
        asyncio.create_task(heartbeat())
        # 先接受连接再预热：第一次听写不用等模型冷启动。开机自启时尤其明显。
        # 预热失败只是慢一点，不能让引擎起不来。
        asyncio.create_task(warmup(asr, llm))
        # 显存是别的程序在抢，情况随时在变。后台盯着：空出来就把模型搬回显卡，
        # 被占满就搬回内存，全程不打扰用户。
        asyncio.create_task(device_monitor(asr, llm, scheduler))
        # keep the server alive forever — a bare asyncio.Future() can exit
        # when the future is never awaited; a sleep loop cannot.
        while True:
            await asyncio.sleep(3600)


if __name__ == "__main__":
    asyncio.run(main())
