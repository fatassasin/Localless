import argparse
import hashlib
import json
import os
import shutil
import sys
import urllib.request
from pathlib import Path

REGISTRY = Path(__file__).with_name("model-registry.json")


def emit(**data):
    print(json.dumps(data), flush=True)


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(4 * 1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def download(url, part, expected, done_before, total, model_id):
    offset = part.stat().st_size if part.exists() else 0
    if offset > expected:
        part.unlink(); offset = 0
    for _ in range(2):
        request = urllib.request.Request(url, headers={"Range": f"bytes={offset}-"} if offset else {})
        with urllib.request.urlopen(request, timeout=45) as response:
            if offset and (response.status != 206 or not response.headers.get("Content-Range", "").startswith(f"bytes {offset}-")):
                part.unlink(missing_ok=True); offset = 0; continue
            with part.open("ab" if offset else "wb") as out:
                got = offset
                while True:
                    chunk = response.read(1024 * 1024)
                    if not chunk:
                        break
                    out.write(chunk); got += len(chunk)
                    emit(modelId=model_id, status="downloading", file=part.stem,
                         bytes=done_before + got, total=total,
                         pct=round((done_before + got) * 100 / total, 1))
            return
    raise RuntimeError(f"server refused resume for {part.name}")


def main():
    registry = json.loads(REGISTRY.read_text(encoding="utf-8"))["models"]
    downloadable = {m["id"]: m for m in registry if m.get("files") and m.get("base")}
    p = argparse.ArgumentParser()
    p.add_argument("--model", choices=downloadable, required=True)
    p.add_argument("--models-dir", required=True)
    args = p.parse_args(); meta = downloadable[args.model]
    root = Path(args.models_dir).resolve()
    final = (root / meta["installDir"]).resolve()
    if root not in final.parents:
        raise RuntimeError("model path escapes models directory")
    temp = final.with_name(final.name + ".part")
    temp.mkdir(parents=True, exist_ok=True)
    total = sum(v[0] for v in meta["files"].values()); done = 0
    base = meta["base"].replace("{revision}", meta["revision"])
    for name, (size, sha) in meta["files"].items():
        target = temp / name; part = temp / (name + ".part")
        if target.exists() and target.stat().st_size == size and digest(target) == sha:
            done += size; continue
        target.unlink(missing_ok=True)
        download(f"{base}/{name}", part, size, done, total, args.model)
        if part.stat().st_size != size or digest(part) != sha:
            raise RuntimeError(f"verification failed: {name}")
        os.replace(part, target); done += size
    (temp / ".complete").write_text(meta["revision"], encoding="ascii")
    if final.exists():
        shutil.rmtree(final)
    final.parent.mkdir(parents=True, exist_ok=True)
    os.replace(temp, final)
    emit(modelId=args.model, status="ready", path=str(final), total=total)


if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        emit(status="error", error=str(e)); sys.exit(1)
