#!/usr/bin/env python3
import hashlib
import json
import platform
import subprocess
import sys

report_path, commit = sys.argv[1], sys.argv[2]
with open(report_path, "r", encoding="utf-8") as handle:
    report = json.load(handle)
if report.get("state") != "complete":
    raise SystemExit("run report is not complete")
url = report.get("conversation_url") or ""
if "/c/" not in url:
    raise SystemExit("run report lacks canonical conversation URL")
payload = {
    "schema": "fabushi.authenticated-linux-e2e.v1",
    "commit": commit,
    "platform": platform.platform(),
    "machine": platform.machine(),
    "chromium": subprocess.run(
        ["sh", "-lc", "google-chrome --version 2>/dev/null || google-chrome-stable --version 2>/dev/null || chromium --version 2>/dev/null || true"],
        text=True, capture_output=True, check=False,
    ).stdout.strip(),
    "conversation_url": url,
    "assistant_text_sha256": hashlib.sha256(report.get("assistant_text", "").encode("utf-8")).hexdigest(),
    "terminal_message": report.get("message"),
    "counters": {
        "approvals_clicked": report.get("approvals_clicked"),
        "recoveries": report.get("recoveries"),
        "rate_limit_pauses": report.get("rate_limit_pauses"),
        "dispatch_retries": report.get("dispatch_retries"),
        "continuations": report.get("continuations"),
    },
}
print(json.dumps(payload, ensure_ascii=False, sort_keys=True))
