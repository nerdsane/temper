#!/usr/bin/env python3
"""Package a Linux build and standalone Observe into a secret-free image context."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

p = argparse.ArgumentParser()
p.add_argument("--binary", type=Path, required=True)
p.add_argument("--z3-lib", type=Path, required=True)
p.add_argument("--node", type=Path, required=True)
p.add_argument("--output", type=Path, required=True)
a = p.parse_args()
r = Path(__file__).resolve().parents[2]
d = a.output.resolve()
d.mkdir(parents=True, exist_ok=False)
for name in ("Dockerfile", "start.sh", "nginx.conf"):
    shutil.copy2(Path(__file__).parent / name, d / name)
for src, name in ((a.binary, "temper"), (a.node, "node")):
    shutil.copy2(src.resolve(), d / name)
for name in ("libz3.so", "libz3.so.4"):
    shutil.copy2((a.z3_lib / name).resolve(), d / name)
u = r / "ui/observe"
shutil.copytree(u / ".next/standalone", d / "observe")
shutil.copytree(u / ".next/static", d / "observe/.next/static")
shutil.copytree(u / "public", d / "observe/public")
metadata = {
    "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=r, text=True).strip(),
    "binary_sha256": hashlib.sha256(a.binary.read_bytes()).hexdigest(),
}
(d / "build.json").write_text(json.dumps(metadata, indent=2) + "\n")
print(d)
