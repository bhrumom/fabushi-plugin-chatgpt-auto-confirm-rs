#!/usr/bin/env python3
"""Fail closed before writes; reserve max(1 GiB, 5% of capacity) per mount."""
import argparse
import json
import os
from pathlib import Path


def check(path: Path, planned: int) -> dict:
    if planned < 0:
        raise ValueError("planned peak must be nonnegative")
    stat = os.statvfs(path)
    total = stat.f_blocks * stat.f_frsize
    free = stat.f_bavail * stat.f_frsize
    if total <= 0:
        raise RuntimeError("filesystem did not report usable capacity")
    reserve = max(1024**3, (total + 19) // 20)
    result = {"path": str(path.resolve()), "free_bytes": free, "reserve_bytes": reserve,
              "estimated_additional_peak_bytes": planned, "safe": free - planned >= reserve}
    if not result["safe"]:
        raise RuntimeError(json.dumps(result))
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--path", action="append", type=Path, default=[])
    parser.add_argument("--planned-bytes", type=int, required=True)
    args = parser.parse_args()
    for path in args.path or [Path.cwd()]:
        print(json.dumps(check(path, args.planned_bytes)))
