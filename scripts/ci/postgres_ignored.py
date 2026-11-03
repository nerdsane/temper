#!/usr/bin/env python3
"""Select the #[ignore] tests the Postgres job runs, for .github/workflows/ci.yml.

The rule: a test ignored with a reason that names a database URL variable
(DATABASE_VARS) needs a disposable PostgreSQL, so the job that has one runs it
with `--run-ignored only`. The reason text is the marker, so a new test ignored
the same way is picked up without a list to keep.

Reads PACKAGES (space-separated packages this run tests, from scripts/ci/affected.py)
and writes to $GITHUB_OUTPUT (stdout when unset):
  args    cargo arguments selecting the packages that hold such tests (empty for none)
  filter  nextest filter expression naming each such test (empty for none)
"""

import os
import re
import sys

DATABASE_VARS = ("TEMPER_TEST_POSTGRES_URL", "TEST_POLICY_DATABASE_URL")
IGNORE = re.compile(r'#\[ignore\s*=\s*"([^"]*)"\]')
FN = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)")
PACKAGE_NAME = re.compile(r'^name\s*=\s*"([^"]+)"', re.MULTILINE)


def package_of(crate_dir):
    with open(os.path.join(crate_dir, "Cargo.toml")) as manifest:
        match = PACKAGE_NAME.search(manifest.read())
    if not match:
        sys.exit(f"no package name in {crate_dir}/Cargo.toml")
    return match.group(1)


def ignored_database_tests(root):
    found = {}
    crates = os.path.join(root, "crates")
    for crate in sorted(os.listdir(crates)):
        crate_dir = os.path.join(crates, crate)
        if not os.path.isfile(os.path.join(crate_dir, "Cargo.toml")):
            continue
        for dirpath, dirnames, filenames in os.walk(crate_dir):
            dirnames[:] = sorted(d for d in dirnames if d != "target")
            for filename in sorted(f for f in filenames if f.endswith(".rs")):
                with open(os.path.join(dirpath, filename)) as source:
                    text = source.read()
                for ignore in IGNORE.finditer(text):
                    if not any(var in ignore.group(1) for var in DATABASE_VARS):
                        continue
                    name = FN.search(text, ignore.end())
                    if not name:
                        sys.exit(f"no test function after {ignore.group(0)} in {dirpath}/{filename}")
                    found.setdefault(package_of(crate_dir), []).append(name.group(1))
    return found


def main():
    root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    tested = set(os.environ.get("PACKAGES", "").split())
    found = {pkg: names for pkg, names in ignored_database_tests(root).items() if pkg in tested}
    args = " ".join(f"-p {pkg}" for pkg in sorted(found))
    names = sorted({name for names in found.values() for name in names})
    # Match the function name at the end of the full test path (module::name).
    filter_expr = " | ".join(f"test(/(^|::){name}$/)" for name in names)
    for pkg in sorted(found):
        print(f"{pkg}: {', '.join(sorted(found[pkg]))}", file=sys.stderr)
    lines = f"args={args}\nfilter={filter_expr}\n"
    target = os.environ.get("GITHUB_OUTPUT")
    if target:
        with open(target, "a") as out:
            out.write(lines)
    else:
        sys.stdout.write(lines)


if __name__ == "__main__":
    main()
