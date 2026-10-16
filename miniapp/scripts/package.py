#!/usr/bin/env python3
"""Prepare deterministic OMA resources. Signing is an external Registry action."""
import argparse
import base64
import hashlib
import json
from pathlib import Path
import shutil
import zipfile

ROOT = Path(__file__).resolve().parents[2]
DOMAIN = b"fabushi.fabapp.manifest.v1\0"
MIME = {".html": "text/html", ".css": "text/css", ".js": "text/javascript",
        ".wasm": "application/wasm", ".txt": "text/plain"}


def canonical(value):
    # All schema and resource keys are ASCII; numbers are bounded integers.
    return json.dumps(value, sort_keys=True, ensure_ascii=False,
                      separators=(",", ":"), allow_nan=False).encode("utf-8")


def prepare(pkg, output, source_commit):
    if len(source_commit) != 40 or any(c not in "0123456789abcdef" for c in source_commit):
        raise ValueError("source commit must be an exact SHA")
    if output.exists():
        raise ValueError("output already exists; never overwrite package preparation")
    output.mkdir(parents=True)
    for item in (ROOT / "miniapp/web").iterdir():
        if item.is_file():
            shutil.copyfile(item, output / item.name)
    (output / "pkg").mkdir()
    for name in ["chatgpt_auto_confirm_web_adapter.js", "chatgpt_auto_confirm_web_adapter_bg.wasm"]:
        shutil.copyfile(pkg / name, output / "pkg" / name)
    wasm = (pkg / "chatgpt_auto_confirm_web_adapter_bg.wasm").read_bytes()
    # JS module delivers local Wasm bytes without fetch/connect permission.
    (output / "wasm-bytes.js").write_text(
        "export const wasmBytes = Uint8Array.from(atob('" + base64.b64encode(wasm).decode("ascii")
        + "'), c => c.charCodeAt(0));\n", encoding="utf-8")
    shutil.copyfile(ROOT / "LICENSE", output / "LICENSE.txt")
    shutil.copyfile(ROOT / "miniapp/wasm/dist/THIRD-PARTY-NOTICES.txt", output / "THIRD-PARTY-NOTICES.txt")
    shutil.copyfile(ROOT / "miniapp/wasm/Cargo.lock", output / "WebCargoLock.txt")
    (output / "BUILD-INFO.txt").write_text(
        f"source={source_commit}\nnative-base=79b432dbe2ba007bb83602c46b759b4490cce17e\n"
        "kind=offline-web-preview\nlicense=MIT\n"
        "live-cdp=unavailable\n", encoding="utf-8")
    files = {}
    for path in sorted(output.rglob("*")):
        if not path.is_file():
            continue
        name = path.relative_to(output).as_posix()
        if not name.isascii() or path.suffix not in MIME:
            raise ValueError("unsupported resource name/MIME")
        data = path.read_bytes()
        files[name] = {"sha256": hashlib.sha256(data).hexdigest(),
                       "bytes": len(data), "mime": MIME[path.suffix]}
    manifest = {
        "manifestVersion": 1, "packageClass": "catalog",
        "appId": "org.fabushi.chatgpt-auto-confirm", "publisherId": "bhrumom",
        "version": "0.1.0", "displayName": "ChatGPT Auto-Confirm 离线工作台",
        "entrypoint": "index.html", "runtime": "web", "minHostApi": 1,
        "offlineCore": True, "requestedCapabilities": [],
        "networkPolicy": {"mode": "deny", "allowedOrigins": []}, "files": files,
    }
    (output / "manifest.unsigned.json").write_bytes(canonical(manifest))
    return manifest


def archive(resources, manifest, destination):
    signed = "signature" in manifest
    if destination.suffix != (".fabapp" if signed else ".zip"):
        raise ValueError("unsigned preparation must be .zip; signed package must be .fabapp")
    if destination.exists():
        raise ValueError("immutable package destination already exists")
    with zipfile.ZipFile(destination, "x", compression=zipfile.ZIP_STORED) as bundle:
        for name, data in [("manifest.json", canonical(manifest))] + [
                (name, (resources / name).read_bytes()) for name in sorted(manifest["files"])]:
            metadata = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
            metadata.external_attr = 0o100644 << 16
            metadata.create_system = 3
            bundle.writestr(metadata, data)
    digest = hashlib.sha256(destination.read_bytes()).hexdigest()
    destination.with_suffix(destination.suffix + ".sha256").write_text(
        f"{digest}  {destination.name}\n", encoding="utf-8")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--pkg", type=Path, required=True)
    parser.add_argument("--resources", type=Path, required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    manifest = prepare(args.pkg, args.resources, args.commit)
    archive(args.resources, manifest, args.output)


if __name__ == "__main__":
    main()
