#!/usr/bin/env python3
"""Create one fresh deployment. Refuses to reuse a volume or sandbox name."""
import argparse
import base64
import json
import os
from pathlib import Path
import secrets
import shlex
import subprocess

p = argparse.ArgumentParser()
p.add_argument("--tl", default="tl")
p.add_argument("--name", required=True)
p.add_argument("--image", required=True)
p.add_argument("--credentials-dir", type=Path, required=True)
a = p.parse_args()
os.umask(0o077)
a.credentials_dir.mkdir(parents=True, exist_ok=False)
def tl(*args):
    return subprocess.check_output([a.tl, *args], text=True).strip()
# Create-only is intentional: existing volumes may already have a writer.
volume = a.name + "-data"
print(tl("fs", "create", volume), flush=True)
keys = {
    "TEMPER_API_KEY": "tmpr_" + secrets.token_urlsafe(32),
    "TEMPER_VAULT_KEY": base64.b64encode(secrets.token_bytes(32)).decode(),
    "AUTH_SECRET": secrets.token_urlsafe(32),
}
password = secrets.token_urlsafe(24)
# Hash through stdin, keeping the Observe password out of command arguments.
hashed = subprocess.check_output(["openssl", "passwd", "-6", "-stdin"], input=password + "\n", text=True).strip()
(a.credentials_dir / "runtime.env").write_text("".join(k + "=" + shlex.quote(v) + "\n" for k, v in keys.items()))
(a.credentials_dir / "observe.htpasswd").write_text("temper:" + hashed + "\n")
(a.credentials_dir / "credentials.json").write_text(json.dumps({"api_key": keys["TEMPER_API_KEY"], "observe_username": "temper", "observe_password": password}, indent=2) + "\n")
sbx = tl("sbx", "create", a.name, "--image", a.image, "--cpus", "2", "--memory", "4096", "--timeout", "600", "--filesystem", volume + ":/var/lib/temper")
(a.credentials_dir / "deployment.json").write_text(json.dumps({"sandbox": sbx, "name": a.name, "volume": volume, "image": a.image}, indent=2) + "\n")
print("Created sandbox " + sbx, flush=True)
for name in ("runtime.env", "observe.htpasswd"):
    tl("sbx", "cp", str(a.credentials_dir / name), a.name + ":/etc/temper/" + name)
print(tl("sbx", "exec", a.name, "sh", "-c", "chmod 600 /etc/temper/runtime.env; chmod 644 /etc/temper/observe.htpasswd"), flush=True)
print(tl("sbx", "exec", "--detach", "--name", "temper", "--restart", "on-failure", "--max-restarts", "5", a.name, "/opt/temper/start.sh"), flush=True)
print(tl("sbx", "exec", a.name, "python3", "-c", Path(__file__).with_name("bootstrap.py").read_text()), flush=True)
print("Verify health and authorization before exposing ports 3000 and 8080.")
print("Credentials saved privately to " + str(a.credentials_dir))
