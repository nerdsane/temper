#!/usr/bin/env python3
"""Runs `cargo test` in every Cargo workspace of the repository other than the root one.

The root workspace's members are tested by the Tests jobs. Every os-app module, GEPA module, test fixture and
reference-app module outside it is a workspace of its own, which no `-p` from the root reaches, so a stale Cargo.lock
or a compile error there went unseen (21 os-app and GEPA locks were stale from 24 Apr to 28 Oct 2026). Cargo itself
says which workspace each Cargo.toml on disk belongs to, so a new module is tested without anyone naming it here. A
package cargo refuses to place (one inside the root's folders that is neither a member nor its own workspace) fails
the run.

Each workspace runs `cargo test --workspace --no-fail-fast`, with `--locked` when it has a Cargo.lock. They share one
target folder (target/workspaces, or $CARGO_TARGET_DIR), so a dependency they share is built once.

usage: workspaces.py [--list]

Each workspace's result goes to $GITHUB_STEP_SUMMARY when it is set, and the run fails if any workspace failed.
"""
import json
import os
import re
import subprocess
import sys
import time

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
SKIP_DIRS = {"target", "node_modules", ".git"}
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def metadata(directory):
    out = subprocess.run(["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd=directory,
                         capture_output=True, text=True)
    if out.returncode:
        return None, out.stderr.strip()
    return json.loads(out.stdout), ""


def workspaces():
    """(workspaces, refused): every workspace root below the repository's other than its own, and every directory
    whose Cargo.toml cargo refuses, each relative to the repository and sorted."""
    root, _ = metadata(ROOT)
    members = {os.path.dirname(p["manifest_path"]) for p in root["packages"]}
    found, refused = set(), {}
    for path, dirs, files in os.walk(ROOT):
        dirs[:] = sorted(d for d in dirs if d not in SKIP_DIRS)
        if "Cargo.toml" not in files or path == ROOT or path in members:
            continue
        meta, error = metadata(path)
        if meta is None:
            refused[os.path.relpath(path, ROOT)] = error
        elif os.path.realpath(meta["workspace_root"]) != os.path.realpath(ROOT):
            found.add(os.path.relpath(meta["workspace_root"], ROOT))
    return sorted(found), dict(sorted(refused.items()))


def run(workspace, target_dir):
    cmd = ["cargo", "test", "--workspace", "--no-fail-fast"]
    if os.path.exists(os.path.join(ROOT, workspace, "Cargo.lock")):
        cmd.append("--locked")
    print(f"::group::{workspace}: {' '.join(cmd)}", flush=True)
    start = time.monotonic()
    proc = subprocess.Popen(cmd, cwd=os.path.join(ROOT, workspace), env=dict(os.environ, CARGO_TARGET_DIR=target_dir),
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors="replace")
    failed, build_error = [], False
    for line in proc.stdout:
        sys.stdout.write(line)
        plain = ANSI.sub("", line).rstrip()
        m = re.match(r"^---- (\S+) stdout ----$", plain)
        if m:
            failed.append(m.group(1))
        if plain.startswith("error"):
            build_error = True
    code = proc.wait()
    print("::endgroup::", flush=True)
    status = "passed" if code == 0 else "failed" if failed else "build failed" if build_error else f"exit {code}"
    return {"workspace": workspace, "status": status, "seconds": time.monotonic() - start, "failed": failed}


def main(argv):
    found, refused = workspaces()
    if "--list" in argv:
        print("\n".join(found + [f"refused: {path}" for path in refused]))
        return 0
    target_dir = os.environ.get("CARGO_TARGET_DIR") or os.path.join(ROOT, "target", "workspaces")
    results = [run(w, target_dir) for w in found]
    results += [{"workspace": path, "status": "refused", "seconds": 0, "failed": []} for path in refused]
    bad = [r for r in results if r["status"] != "passed"]
    lines = [f"### cargo test, every other workspace: {len(results) - len(bad)} of {len(results)} passed", "",
             "| Workspace | Result | Seconds | Failing tests |", "|---|---|---:|---|"]
    for r in sorted(results, key=lambda r: (r["status"] == "passed", -r["seconds"])):
        lines.append(f"| {r['workspace']} | {r['status']} | {r['seconds']:.0f} | {', '.join(r['failed'])} |")
        print(f"RESULT {r['workspace']} {r['status'].replace(' ', '-')} {r['seconds']:.0f}s {' '.join(r['failed'])}")
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as f:
            f.write("\n".join(lines) + "\n")
    for path, error in refused.items():
        print(f"::error::{path}: cargo refuses its Cargo.toml: {error.splitlines()[0] if error else ''}")
    for r in bad:
        if r["status"] != "refused":
            print(f"::error::{r['workspace']}: {r['status']} {' '.join(r['failed'])}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
