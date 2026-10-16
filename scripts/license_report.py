#!/usr/bin/env python3
# Bundle original registry license/notices from the locked resolved dependency graph.
import json
from pathlib import Path
import subprocess
from disk_guard import check
check(Path.cwd(), 40 * 1024**2)
data = json.loads(subprocess.check_output(["cargo", "metadata", "--locked", "--format-version", "1"]))
sections = []
inventory = []
size = 0
for package in sorted(data["packages"], key=lambda p: (p["name"], p["version"])):
    if package["source"] is None:
        continue
    root = Path(package["manifest_path"]).parent.resolve()
    paths = set()
    for pattern in ["LICENSE*", "COPYING*", "NOTICE*"]:
        paths.update(p for p in root.glob(pattern) if p.is_file())
    if package.get("license_file"):
        paths.add(root / package["license_file"])
    text = []
    for path in sorted(paths):
        if not path.resolve().is_relative_to(root) or path.stat().st_size > 1024**2:
            raise RuntimeError(f"unbounded/untrusted license path for {package['name']}")
        value = path.read_text(encoding="utf-8")
        size += len(value.encode())
        if size > 32 * 1024**2:
            raise RuntimeError("third-party notice budget exceeded")
        text.append(f"--- {path.name} ---\n{value}")
    inventory.append({"name": package["name"], "version": package["version"], "license": package["license"], "repository": package["repository"], "license_files": [p.name for p in sorted(paths)]})
    sections.append(f"\n=== {package['name']} {package['version']} | {package['license']} ===\n" + "\n".join(text))
Path("dist/THIRD-PARTY-NOTICES.txt").write_text("Published dependency notices; Cargo.lock identifies exact sources.\n" + "\n".join(sections))
Path("dist/license-inventory.json").write_text(json.dumps(inventory, indent=2) + "\n")
print(json.dumps({"licensed_packages": len(inventory), "notice_bytes": size}))
