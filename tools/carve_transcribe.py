"""把碎出来的候选 wav 切片送进还活着的引擎做识别。

切成 40 秒一片是必须的：引擎 finalize 里那句 asyncio.wait_for(transcribe,
timeout=60) 不随音频长度放大，整段 130 秒送进去会重演这次事故。
"""
import asyncio, json, os, sys, struct, wave, websockets

WS = "ws://127.0.0.1:8765/ws/rt_voice_flow"
SR = 16000
SLICE_S = 40


def read_pcm(path):
    with wave.open(path, "rb") as w:
        assert w.getframerate() == SR and w.getsampwidth() == 2 and w.getnchannels() == 1
        return w.readframes(w.getnframes())


def wav_bytes(pcm):
    return (b"RIFF" + struct.pack("<I", 36 + len(pcm)) + b"WAVEfmt "
            + struct.pack("<IHHIIHH", 16, 1, 1, SR, SR * 2, 2, 16)
            + b"data" + struct.pack("<I", len(pcm)) + pcm)


def frame(aid: str, payload: bytes) -> bytes:
    a = aid.encode()
    return bytes([76, 76, 65, 70, 1, len(a)]) + a + payload


async def one(ws, aid, pcm):
    await ws.send(json.dumps({
        "type": "start_audio", "audio_id": aid, "mode": "transcript",
        "history": [], "corrections": [], "focused_text": "", "target_app": "",
        "audio_metadata": {"audio_format": "wav", "audio_sample_rate": SR, "audio_channels": 1}}))
    await ws.send(frame(aid, wav_bytes(pcm)))
    await ws.send(json.dumps({"type": "end_audio", "audio_id": aid, "mode": "transcript",
                              "total_duration": len(pcm) / 2 / SR}))
    while True:
        m = json.loads(await asyncio.wait_for(ws.recv(), timeout=180))
        if m.get("audio_id") != aid:
            continue
        if m.get("type") == "refine_completed":
            return m.get("refined_text") or m.get("raw_text") or ""
        if m.get("type") == "error":
            return f"[错误: {m.get('message')}]"


async def main(paths):
    async with websockets.connect(WS, max_size=None) as ws:
        for p in paths:
            pcm = read_pcm(p)
            total = len(pcm) / 2 / SR
            print(f"\n=== {os.path.basename(p)}  {total:.1f}s ===", flush=True)
            step = SLICE_S * SR * 2
            for i in range(0, len(pcm), step):
                part = pcm[i:i + step]
                if len(part) < SR * 2:
                    continue
                aid = f"carve-{os.path.basename(p)[:12]}-{i//step}"
                try:
                    txt = await one(ws, aid, part)
                except Exception as e:
                    txt = f"[异常 {e!r}]"
                print(f"  [{i/(SR*2):5.0f}s+] {txt}", flush=True)


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1:]))
