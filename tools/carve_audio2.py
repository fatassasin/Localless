"""第二轮：不再要求连续，改成按地址聚类；同时试 int16 和 float32 两种解释。

第一轮 156 个命中却没有 >=2 秒的连续段，是因为语音里的静音会把一段录音切成
很多碎片。这里把命中窗口按地址聚成簇（允许 128 KB 的空隙），一段 162 秒的
录音应该表现为一个跨度 ~5 MB、命中密度很高的簇。
"""
import ctypes, ctypes.wintypes as w, sys, os, struct, numpy as np

PROCESS_QUERY_INFORMATION, PROCESS_VM_READ = 0x0400, 0x0010
MEM_COMMIT, MEM_PRIVATE = 0x1000, 0x20000
WRITABLE = {0x04, 0x40, 0x08, 0x80}
k32 = ctypes.WinDLL("kernel32", use_last_error=True)


class MBI(ctypes.Structure):
    _fields_ = [("BaseAddress", ctypes.c_void_p), ("AllocationBase", ctypes.c_void_p),
                ("AllocationProtect", w.DWORD), ("__align", w.DWORD),
                ("RegionSize", ctypes.c_size_t), ("State", w.DWORD),
                ("Protect", w.DWORD), ("Type", w.DWORD), ("__align2", w.DWORD)]


k32.OpenProcess.restype = w.HANDLE
k32.VirtualQueryEx.restype = ctypes.c_size_t
k32.ReadProcessMemory.restype = w.BOOL

WIN = 8192            # 字节；int16 下 4096 采样 = 0.256s
CORR_MIN = 0.80
GAP = 256 * 1024      # 簇内允许的空隙


def hits_i16(block):
    n = len(block) // 2
    per = WIN // 2
    nw = n // per
    if nw == 0:
        return np.zeros(0, bool)
    x = np.frombuffer(block[: nw * per * 2], np.int16).astype(np.float32).reshape(nw, per)
    x = x - x.mean(axis=1, keepdims=True)
    rms = np.sqrt((x * x).mean(axis=1)) + 1e-9
    corr = (x[:, :-1] * x[:, 1:]).mean(axis=1) / (rms * rms)
    return (corr > CORR_MIN) & (rms > 60.0) & (rms < 20000.0)


def hits_f32(block):
    n = len(block) // 4
    per = WIN // 4
    nw = n // per
    if nw == 0:
        return np.zeros(0, bool)
    x = np.frombuffer(block[: nw * per * 4], np.float32).reshape(nw, per).astype(np.float64)
    ok = np.isfinite(x).all(axis=1) & (np.abs(x).max(axis=1) <= 1.5)
    x = np.nan_to_num(x) - np.nan_to_num(x).mean(axis=1, keepdims=True)
    rms = np.sqrt((x * x).mean(axis=1)) + 1e-12
    corr = (x[:, :-1] * x[:, 1:]).mean(axis=1) / (rms * rms)
    return ok & (corr > CORR_MIN) & (rms > 1e-3) & (rms < 0.8)


def scan(h, detector, label):
    mbi = MBI(); addr = 0; marks = []
    buf = ctypes.create_string_buffer(4 * 1024 * 1024); got = ctypes.c_size_t()
    while k32.VirtualQueryEx(h, ctypes.c_void_p(addr), ctypes.byref(mbi), ctypes.sizeof(mbi)):
        base, size = mbi.BaseAddress or 0, mbi.RegionSize
        nxt = base + size
        if (mbi.State == MEM_COMMIT and mbi.Type == MEM_PRIVATE
                and mbi.Protect in WRITABLE and size >= WIN):
            off = 0
            while off < size:
                chunk = min(len(buf), size - off)
                if not k32.ReadProcessMemory(h, ctypes.c_void_p(base + off), buf,
                                             chunk, ctypes.byref(got)) or got.value == 0:
                    off += chunk; continue
                data = buf.raw[: got.value]
                for i, ok in enumerate(detector(data)):
                    if ok:
                        marks.append(base + off + i * WIN)
                off += got.value
        addr = nxt
        if addr >= (1 << 47):
            break
    print(f"[{label}] 命中窗口 {len(marks)} 个 = {len(marks)*WIN/1e6:.2f} MB")
    clusters = []
    for a in marks:
        if clusters and a - clusters[-1][1] <= GAP:
            clusters[-1][1] = a + WIN
            clusters[-1][2] += 1
        else:
            clusters.append([a, a + WIN, 1])
    clusters.sort(key=lambda c: -(c[1] - c[0]))
    return clusters


def dump(h, a, n, path, fmt):
    got = ctypes.c_size_t()
    out = ctypes.create_string_buffer(n)
    if not k32.ReadProcessMemory(h, ctypes.c_void_p(a), out, n, ctypes.byref(got)):
        return None
    raw = out.raw[: got.value]
    if fmt == "f32":
        x = np.frombuffer(raw[: len(raw)//4*4], np.float32)
        pcm = (np.clip(np.nan_to_num(x), -1, 1) * 32767).astype(np.int16).tobytes()
    else:
        pcm = raw[: len(raw)//2*2]
    with open(path, "wb") as f:
        f.write(b"RIFF" + struct.pack("<I", 36 + len(pcm)) + b"WAVEfmt ")
        f.write(struct.pack("<IHHIIHH", 16, 1, 1, 16000, 32000, 2, 16))
        f.write(b"data" + struct.pack("<I", len(pcm)) + pcm)
    return len(pcm) / 2 / 16000


def main(pid, outdir):
    h = k32.OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, False, pid)
    if not h:
        print("OpenProcess 失败", ctypes.get_last_error()); return 1
    os.makedirs(outdir, exist_ok=True)
    for label, det, fmt in (("int16", hits_i16, "i16"), ("float32", hits_f32, "f32")):
        cl = scan(h, det, label)
        print(f"[{label}] 最大的 8 个簇（跨度 / 命中窗口数 / 折算秒数）：")
        for i, (a, b, cnt) in enumerate(cl[:8]):
            span = b - a
            secs = span / (2 if fmt == "i16" else 4) / 16000
            print(f"  0x{a:012x}  跨度 {span/1e6:7.2f} MB  命中 {cnt:5d}  ≈{secs:7.1f}s")
            if span >= 400_000:
                p = os.path.join(outdir, f"{label}_{i:02d}_{secs:.0f}s.wav")
                d = dump(h, a, span, p, fmt)
                if d:
                    print(f"       -> {os.path.basename(p)}  ({d:.1f}s)")
    return 0


if __name__ == "__main__":
    sys.exit(main(int(sys.argv[1]), sys.argv[2]))
