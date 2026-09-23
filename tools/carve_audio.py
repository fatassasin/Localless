"""只读扫描活着的引擎进程内存，捞出看起来像 16kHz 单声道 pcm16 语音的段。

判据：语音在 16kHz 上被严重过采样，相邻采样的一阶自相关 >0.9；模型权重
（float32/bf16 重解释成 int16）相邻值基本不相关，接近 0。用这一条就能把
5 MB 的录音从 17 GB 权重里挑出来。全程 ReadProcessMemory，不写目标进程。
"""
import ctypes, ctypes.wintypes as w, sys, os, numpy as np

PROCESS_QUERY_INFORMATION = 0x0400
PROCESS_VM_READ = 0x0010
MEM_COMMIT, MEM_PRIVATE = 0x1000, 0x20000
WRITABLE = {0x04, 0x40, 0x08, 0x80}  # READWRITE, EXECUTE_READWRITE, WRITECOPY, EXECUTE_WRITECOPY

k32 = ctypes.WinDLL("kernel32", use_last_error=True)


class MBI(ctypes.Structure):
    _fields_ = [("BaseAddress", ctypes.c_void_p), ("AllocationBase", ctypes.c_void_p),
                ("AllocationProtect", w.DWORD), ("__align", w.DWORD),
                ("RegionSize", ctypes.c_size_t), ("State", w.DWORD),
                ("Protect", w.DWORD), ("Type", w.DWORD), ("__align2", w.DWORD)]


k32.OpenProcess.restype = w.HANDLE
k32.VirtualQueryEx.restype = ctypes.c_size_t
k32.ReadProcessMemory.restype = w.BOOL

WIN = 16384          # 扫描窗口字节数（8192 采样 = 0.5s）
CORR_MIN = 0.90      # 一阶自相关下限
RMS_MIN = 120.0      # 太安静的段（全零、稀疏权重）不要
RMS_MAX = 20000.0    # 削顶/噪声不要


def speechlike(block: bytes):
    """把一块字节切成窗口逐个判定，返回 bool 数组（每窗口一个）。"""
    n = len(block) // 2
    if n < WIN // 2:
        return np.zeros(0, bool)
    x = np.frombuffer(block[: n * 2], np.int16).astype(np.float32)
    per = WIN // 2
    nw = n // per
    if nw == 0:
        return np.zeros(0, bool)
    x = x[: nw * per].reshape(nw, per)
    x = x - x.mean(axis=1, keepdims=True)
    rms = np.sqrt((x * x).mean(axis=1)) + 1e-9
    a, b = x[:, :-1], x[:, 1:]
    corr = (a * b).mean(axis=1) / (rms * rms)
    return (corr > CORR_MIN) & (rms > RMS_MIN) & (rms < RMS_MAX)


def main(pid: int, outdir: str):
    h = k32.OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, False, pid)
    if not h:
        print("OpenProcess 失败", ctypes.get_last_error()); return 1
    os.makedirs(outdir, exist_ok=True)

    mbi = MBI()
    addr = 0
    spans = []          # (起始地址, 字节数)
    scanned = 0
    buf = ctypes.create_string_buffer(4 * 1024 * 1024)
    got = ctypes.c_size_t()

    while k32.VirtualQueryEx(h, ctypes.c_void_p(addr), ctypes.byref(mbi), ctypes.sizeof(mbi)):
        base, size = mbi.BaseAddress or 0, mbi.RegionSize
        nxt = base + size
        if (mbi.State == MEM_COMMIT and mbi.Type == MEM_PRIVATE
                and mbi.Protect in WRITABLE and size >= WIN):
            off = 0
            run_start, run_len = None, 0
            while off < size:
                chunk = min(len(buf), size - off)
                if not k32.ReadProcessMemory(h, ctypes.c_void_p(base + off), buf,
                                             chunk, ctypes.byref(got)) or got.value == 0:
                    off += chunk
                    run_start, run_len = None, 0
                    continue
                data = buf.raw[: got.value]
                scanned += len(data)
                flags = speechlike(data)
                for i, ok in enumerate(flags):
                    a = base + off + i * WIN
                    if ok:
                        if run_start is None:
                            run_start, run_len = a, WIN
                        else:
                            run_len += WIN
                    elif run_start is not None:
                        spans.append((run_start, run_len))
                        run_start, run_len = None, 0
                off += got.value
            if run_start is not None:
                spans.append((run_start, run_len))
        addr = nxt
        if addr >= (1 << 47):
            break

    print(f"扫描了 {scanned/1e9:.1f} GB，命中 {len(spans)} 段")
    spans.sort(key=lambda s: -s[1])
    keep = [s for s in spans if s[1] >= 2 * 16000 * 2]      # 至少 2 秒
    print(f"其中 >=2 秒的 {len(keep)} 段：")
    saved = []
    for idx, (a, n) in enumerate(keep[:12]):
        secs = n / 2 / 16000
        out = ctypes.create_string_buffer(n)
        if not k32.ReadProcessMemory(h, ctypes.c_void_p(a), out, n, ctypes.byref(got)):
            continue
        pcm = out.raw[: got.value]
        path = os.path.join(outdir, f"cand{idx:02d}_{secs:.1f}s.wav")
        with open(path, "wb") as f:
            import struct
            f.write(b"RIFF" + struct.pack("<I", 36 + len(pcm)) + b"WAVEfmt ")
            f.write(struct.pack("<IHHIIHH", 16, 1, 1, 16000, 32000, 2, 16))
            f.write(b"data" + struct.pack("<I", len(pcm)) + pcm)
        print(f"  0x{a:012x}  {secs:8.1f}s  {len(pcm)/1e6:6.2f} MB  -> {os.path.basename(path)}")
        saved.append(path)
    return 0


if __name__ == "__main__":
    sys.exit(main(int(sys.argv[1]), sys.argv[2]))
