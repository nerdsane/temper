#!/usr/bin/env python3
"""Capture per-test-binary and Cargo compiler/link timing baselines."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parent.parent


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--output",
        type=Path,
        default=ROOT / "target" / "validation-reports" / "local-profile.json",
    )
    parser.add_argument(
        "--execute",
        action="store_true",
        help="Run every compiled test binary with the ordinary DST filter after compiling it.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    command = [
        "cargo",
        "test",
        "--workspace",
        "--no-run",
        "--message-format=json",
        "--timings",
    ]
    started = time.monotonic()
    completed = subprocess.run(
        command,
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )
    compile_duration = time.monotonic() - started
    sys.stderr.write(completed.stderr)
    executables: dict[str, dict[str, str]] = {}
    for line in completed.stdout.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("reason") != "compiler-artifact" or not message.get("executable"):
            continue
        profile = message.get("profile", {})
        if not profile.get("test"):
            continue
        target = message["target"]
        executable = message["executable"]
        executables[executable] = {
            "package_id": message["package_id"],
            "target": target["name"],
            "kind": ",".join(target["kind"]),
        }

    records = [
        {
            **identity,
            "duration_seconds": None,
            "exit_code": None,
        }
        for _, identity in sorted(
            executables.items(), key=lambda item: (item[1]["package_id"], item[1]["target"])
        )
    ]
    exit_code = completed.returncode
    if completed.returncode == 0 and args.execute:
        records = []
        for executable, identity in sorted(
            executables.items(), key=lambda item: (item[1]["package_id"], item[1]["target"])
        ):
            binary_started = time.monotonic()
            result = subprocess.run(
                [executable, "--skip", "dst_"],
                cwd=ROOT,
                check=False,
            )
            records.append(
                {
                    **identity,
                    "duration_seconds": round(time.monotonic() - binary_started, 3),
                    "exit_code": result.returncode,
                }
            )
            if result.returncode != 0 and exit_code == 0:
                exit_code = result.returncode

    report = {
        "schema_version": 1,
        "compile_command": command,
        "compile_exit_code": completed.returncode,
        "compile_wall_seconds": round(compile_duration, 3),
        "cargo_timing_report": "target/cargo-timings/cargo-timing.html",
        "test_binary_count": len(executables),
        "test_binaries_executed": args.execute,
        "test_binaries": records,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print(f"Wrote validation profile to {args.output}")
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
