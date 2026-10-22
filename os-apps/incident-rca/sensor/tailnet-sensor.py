#!/usr/bin/env python3
"""Sensor bridge: tailnet-diagnose.py -> Temper incident-rca.

Runs the read-only tunnel classifier. On a non-HEALTHY classification it
files an Incident in the Temper kernel (or reopens the matching Closed
incident for the same failure mode; dedupes against an already-open one).

The catch-22: when the tunnel is down, the VM cannot reach the Mac-hosted
kernel. Undeliverable detections are appended to a local queue file,
timestamped at detection, and flushed on the next successful run.

Usage:
    python3 tailnet-sensor.py [--base http://127.0.0.1:3000]
                              [--queue ~/workspace/state/tailnet-sensor-queue.jsonl]
                              [--diagnose ~/workspace/bin/tailnet-diagnose.py]

Exit code: the classifier's exit code (0 healthy, 1/2/3 fault class).
"""
import argparse
import base64
import json
import os
import subprocess
import sys
import urllib.request
import urllib.error
from datetime import datetime, timezone

NS = "Temper.IncidentRCA"
MODES = {1: "ENROLLMENT_DOWN", 2: "ADDRESS_APPROVAL_LAPSED", 3: "TUNNEL_DOWN"}


def headers():
    h = {"Content-Type": "application/json", "Accept": "application/json",
         "X-Tenant-Id": os.environ.get("TEMPER_TENANT", "default")}
    key = os.environ.get("TEMPER_API_KEY")
    if not key and os.environ.get("TEMPER_API_KEY_FILE"):
        key = open(os.environ["TEMPER_API_KEY_FILE"]).read().strip()
    if key:
        h["Authorization"] = f"Bearer {key}"
    extra = os.environ.get("TEMPER_SENSOR_HEADERS")
    if extra:
        h.update(json.loads(extra))
    return h


def call(base, method, path, body=None):
    req = urllib.request.Request(base + path, method=method,
                                 data=json.dumps(body).encode() if body is not None else None,
                                 headers=headers())
    with urllib.request.urlopen(req, timeout=15) as r:
        raw = r.read().decode()
        return r.status, json.loads(raw) if raw else {}


def file_incident(base, mode, detected_at, detail):
    """File via the Mac-side helper over SSH (the proven channel).

    The helper dedupes/reopens/creates with a fresh supervisor JWT and
    prints one outcome line. Raises when the Mac is unreachable (tunnel
    down) so the caller keeps the detection queued.
    """
    payload = base64.b64encode(json.dumps(
        {"mode": mode, "detected_at": detected_at, "detail": detail}).encode()).decode()
    proc = subprocess.run(
        [os.path.expanduser("~/workspace/bin/mac-ssh.sh"), "bencharney@100.71.118.10",
         f"echo '{payload}' | base64 -d | python3 ~/temper-server/file_incident.py"],
        capture_output=True, text=True, timeout=120)
    if proc.returncode != 0:
        raise RuntimeError(f"mac filer failed: {proc.stderr.strip()[:200]}")
    return proc.stdout.strip()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default=os.environ.get("TEMPER_BASE", "http://127.0.0.1:3000"))
    ap.add_argument("--queue", default=os.path.expanduser("~/workspace/state/tailnet-sensor-queue.jsonl"))
    ap.add_argument("--diagnose", default=os.path.expanduser("~/workspace/bin/tailnet-diagnose.py"))
    args = ap.parse_args()

    cmd = [args.diagnose] if os.access(args.diagnose, os.X_OK) else [sys.executable, args.diagnose]
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    code = proc.returncode
    detail = (proc.stdout or proc.stderr).strip().splitlines()
    detail = detail[-1] if detail else f"classifier exit {code}"
    detected_at = datetime.now(timezone.utc).astimezone().isoformat(timespec="seconds")

    # Flush any queued detections first (they belong to earlier outages).
    queued = []
    if os.path.exists(args.queue):
        queued = [json.loads(l) for l in open(args.queue) if l.strip()]
    pending = queued + ([{"mode": MODES[code], "detected_at": detected_at, "detail": detail}] if code in MODES else [])
    remaining = []
    for item in pending:
        try:
            outcome = file_incident(args.base, item["mode"], item["detected_at"], item["detail"])
            print(f"sensor: {item['mode']} @ {item['detected_at']} -> {outcome}")
        except Exception as e:  # kernel unreachable (tunnel down) — keep queued
            remaining.append(item)
            print(f"sensor: kernel unreachable for {item['mode']} ({e}); queued")
    os.makedirs(os.path.dirname(args.queue), exist_ok=True)
    with open(args.queue, "w") as f:
        for item in remaining:
            f.write(json.dumps(item) + "\n")
    if code == 0:
        print("sensor: HEALTHY")
    return code


if __name__ == "__main__":
    sys.exit(main())
