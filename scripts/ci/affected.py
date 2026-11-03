#!/usr/bin/env python3
"""Decide what a CI run tests, for .github/workflows/ci.yml.

Lane "full" (schedule, a plain dispatch, a first push of a branch) runs every check.
Lane "pr" (a push, a pull request, or a dispatch given changed paths)
tests only the workspace packages a change can reach: the packages whose files
changed plus every package that depends on them (normal, dev and build
dependencies all count), worked out from `cargo metadata`. A change to anything outside a
package that a build or test could read (Cargo.lock, the toolchain, .github/,
test-fixtures/, os-apps/, skills/, scripts/, ...) tests every package; only the
paths in DOCS are known to be read by nothing.

The nightly schedule runs the full lane.

Writes key=value lines to $GITHUB_OUTPUT (stdout when unset):
  lane      full | pr
  mode      full | affected | none
  packages  space-separated package names to test (empty for none)
  test_args cargo arguments that select those packages' tests (empty for none)
  rust      false only when nothing but DOCS changed
  sdk_ts    true when the TypeScript SDK's checks should run: Rust changed (as
            before) or the SDK itself did; packages/ is in DOCS for Rust's sake
  shards    JSON matrix of Tests shards for the pr lane
  specs     true when Spec Verification runs: the full lane, or a changed spec file
  tests     false on a pull request: it only compiles and lints, and the push to
            main that lands it runs every test once (Rita, 31 Oct 2026: "CI 2
            times is insane"); true for every other event
"""

import fnmatch
import json
import os
import subprocess
import sys

# Paths outside every package that no build, test or check reads.
DOCS = [
    "docs/*",
    "*.md",
    "*.pdf",
    "*.png",
    "*.html",
    "LICENSE-*",
    "assets/*",
    "ui/*",
    "packages/*",
    "plugins/*",
    ".agents/*",
    ".codex/*",
    ".cursor/*",
    ".vision/*",
]
SERVER = "temper-server"
# temper-server's tests run with the features the whole workspace turns on for it.
SERVER_FEATURES = "temper-server/observe,temper-server/sim"
# The files Spec Verification reads. temper-cli's verify:: tests walk the whole repository
# for spec collections (a directory holding *.ioa.toml and model.csdl.xml) and read their
# IOA, CSDL, TLA and policies/*.cedar files wherever they sit, so a change to one runs that
# job whatever owns it: reference-apps/*/specs belong to packages that never lead to
# temper-cli, and docs/examples/pipeline-specs is under DOCS.
SPEC_FILES = (".ioa.toml", ".csdl.xml", ".tla", ".cedar")
# Packages whose tests take long enough to be worth two machines.
HEAVY = {"temper-platform", "temper-store-turso", "temper-cli", "temper-actor-runtime"}
# Target kinds `cargo test --doc` runs doctests for.
LIBRARY_KINDS = {"lib", "rlib", "proc-macro"}


def is_docs(path):
    if "/" not in path:
        return any(fnmatch.fnmatch(path, p) for p in DOCS if "/" not in p)
    return any(fnmatch.fnmatch(path, p) for p in DOCS if "/" in p)


def workspace():
    env = dict(os.environ, RUSTUP_TOOLCHAIN=os.environ.get("RUSTUP_TOOLCHAIN", "stable"))
    out = subprocess.check_output(["cargo", "metadata", "--no-deps", "--format-version", "1"], env=env)
    meta = json.loads(out)
    root = meta["workspace_root"].rstrip("/") + "/"
    dirs = {}
    for pkg in meta["packages"]:
        rel = os.path.dirname(pkg["manifest_path"])[len(root):]
        dirs[rel + "/"] = pkg["name"]
    names = set(dirs.values())
    dependents = {name: set() for name in names}
    for pkg in meta["packages"]:
        for dep in pkg["dependencies"]:
            if dep["name"] in names:
                dependents[dep["name"]].add(pkg["name"])
    # Only a library has doctests: `cargo test --doc` refuses a package without one.
    libraries = {pkg["name"] for pkg in meta["packages"]
                 if any(LIBRARY_KINDS & set(target["kind"]) for target in pkg["targets"])}
    return dirs, names, dependents, libraries


def owner(path, dirs):
    best = None
    for prefix, name in dirs.items():
        if path.startswith(prefix) and (best is None or len(prefix) > len(best[0])):
            best = (prefix, name)
    return best[1] if best else None


def closure(start, dependents):
    seen = set(start)
    todo = list(start)
    while todo:
        for nxt in dependents[todo.pop()]:
            if nxt not in seen:
                seen.add(nxt)
                todo.append(nxt)
    return seen


def changed_paths():
    given = os.environ.get("CHANGED_PATHS", "").split()
    if given:
        return given
    base, head = os.environ["BASE_SHA"], os.environ["HEAD_SHA"]
    out = subprocess.check_output(["git", "diff", "--name-only", f"{base}...{head}"], text=True)
    return [line for line in out.splitlines() if line]


def shards(packages, libraries):
    cells = []
    if SERVER in packages:
        for i in (1, 2):
            cells.append({"name": f"temper-server {i}/2", "args": f"-p {SERVER} --features {SERVER_FEATURES}",
                          "partition": f"hash:{i}/2", "gepa": True, "doc": ""})
    rest = sorted(packages - {SERVER})
    if rest:
        count = 2 if rest and HEAVY & set(rest) else 1
        args = " ".join(f"-p {name}" for name in rest)
        for i in range(1, count + 1):
            cells.append({"name": f"other packages {i}/{count}" if count > 1 else "other packages", "args": args,
                          "partition": f"hash:{i}/{count}", "gepa": False, "doc": ""})
    documented = sorted(packages & libraries)
    if documented:
        # Doctests build the libraries with other features than the test binaries,
        # so they get a machine of their own rather than lengthening a shard.
        cells.append({"name": "doctests", "args": "", "partition": "", "gepa": False,
                      "doc": " ".join(f"-p {name}" for name in documented)})
    return cells


def test_args(packages):
    # temper-server gets the features the workspace run turns on for it, as in shards().
    args = " ".join(f"-p {name}" for name in sorted(packages))
    return f"{args} --features {SERVER_FEATURES}" if SERVER in packages else args


def main():
    # A push tests what it moved the branch by; the nightly full lane catches what that misses.
    proposed = os.environ.get("EVENT") == "pull_request"
    pushed = os.environ.get("EVENT") == "push" and os.environ.get("BASE_SHA", "").strip("0")
    lane = "pr" if proposed or pushed or os.environ.get("CHANGED_PATHS", "").strip() else "full"
    dirs, names, dependents, libraries = workspace()
    out = {"lane": lane, "mode": "full", "packages": " ".join(sorted(names)), "test_args": "--workspace", "rust": "true",
           "specs": "true", "tests": "false" if os.environ.get("EVENT") == "pull_request" else "true"}
    if lane == "pr":
        direct, outside, docs_only, sdk_ts, specs = set(), [], True, False, False
        for path in changed_paths():
            sdk_ts = sdk_ts or path.startswith("packages/temper-sdk-ts/")
            specs = specs or path.endswith(SPEC_FILES)
            name = owner(path, dirs)
            if name:
                direct.add(name)
                docs_only = False
            elif not is_docs(path):
                outside.append(path)
                docs_only = False
        if outside:
            print(f"Testing everything: {len(outside)} changed path(s) outside a package, first {outside[0]}", file=sys.stderr)
            packages = set(names)
        else:
            packages = closure(direct, dependents)
            out["mode"] = "affected" if packages else "none"
        out["packages"] = " ".join(sorted(packages))
        out["test_args"] = test_args(packages)
        out["rust"] = "false" if docs_only else "true"
        out["sdk_ts"] = "true" if sdk_ts or not docs_only else "false"
        out["specs"] = "true" if specs else "false"
        out["shards"] = json.dumps({"include": shards(packages, libraries)}) if packages else json.dumps({"include": []})
        print(f"Direct: {sorted(direct)}; testing {len(packages)} package(s): {out['packages'] or 'none'}", file=sys.stderr)
    else:
        out["sdk_ts"] = "true"
        out["shards"] = json.dumps({"include": []})
    lines = "".join(f"{k}={v}\n" for k, v in out.items())
    target = os.environ.get("GITHUB_OUTPUT")
    if target:
        with open(target, "a") as fh:
            fh.write(lines)
    sys.stdout.write(lines)


if __name__ == "__main__":
    main()
