#!/usr/bin/env python3
"""Install a per-user macOS supervisor; existing endpoint leases prevent duplicates."""
import argparse
import os
from pathlib import Path
import plistlib
import shutil
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--binary", type=Path, required=True)
parser.add_argument("--database", type=Path, required=True)
parser.add_argument("--log", type=Path, required=True)
parser.add_argument("--cdp", default="http://127.0.0.1:9223")
args = parser.parse_args()
source_binary = args.binary.resolve(strict=True)
service_dir = Path.home() / "Library" / "Application Support" / "FabushiSupervisor"
service_dir.mkdir(parents=True, exist_ok=True)
binary = service_dir / "fabushi-chatgpt-auto-confirm"
shutil.copy2(source_binary, binary.with_suffix(".new"))
os.replace(binary.with_suffix(".new"), binary)
database = args.database.resolve(strict=True)
log = args.log.absolute()
service_log = service_dir / "supervisor.log"
log.parent.mkdir(parents=True, exist_ok=True)
label = "com.fabushi.chatgpt.supervisor"
plist = Path.home() / "Library" / "LaunchAgents" / (label + ".plist")
plist.parent.mkdir(parents=True, exist_ok=True)
config = {
    "Label": label,
    "ProgramArguments": [str(binary), "--cdp", args.cdp, "--database", str(database), "supervise"],
    "WorkingDirectory": str(service_dir),
    "RunAtLoad": True,
    "KeepAlive": True,
    "ThrottleInterval": 30,
    "StandardOutPath": str(service_log),
    "StandardErrorPath": str(service_log),
}
with plist.open("wb") as stream:
    plistlib.dump(config, stream)
plist.chmod(0o600)
if log != service_log:
    if log.exists() and not log.is_symlink():
        shutil.copy2(log, log.with_suffix(".previous.log"))
    if log.exists() or log.is_symlink():
        log.unlink()
    log.symlink_to(service_log)
domain = "gui/" + str(os.getuid())
subprocess.run(["launchctl", "bootout", domain + "/" + label], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
subprocess.run(["launchctl", "bootstrap", domain, str(plist)], check=True)
print("Installed " + str(plist))
print("Inspect: launchctl print " + domain + "/" + label)
print("Stop: launchctl bootout " + domain + "/" + label)
