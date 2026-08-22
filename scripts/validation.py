#!/usr/bin/env python3
"""Run and validate Temper's canonical validation lanes."""

from __future__ import annotations

import argparse
from functools import lru_cache
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
from typing import Any


ROOT = Path(__file__).resolve().parent.parent
MANIFEST_PATH = ROOT / ".ci" / "validation-lanes.json"
ORDINARY_CATEGORY = "ordinary-test"
FAST_LANES = ("fmt", "check", "clippy", "integrity")
WORKSPACE_SENSITIVE = {
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain",
    "rust-toolchain.toml",
}
SOURCE_PREFIXES = ("crates/", "reference-apps/", "third-party/", "wasm-modules/", "os-apps/")
MATRIX_CATEGORIES = {ORDINARY_CATEGORY, "dst"}
ALLOWED_CATEGORIES = {
    "format",
    "compile",
    "lint",
    "integrity",
    ORDINARY_CATEGORY,
    "backend-parity",
    "feature-test",
    "dst",
    "spec-verification",
    "instrumentation",
    "local-full",
    "bench",
}
CONSERVATIVE_PATH_PREFIXES = (".ci/", ".claude/hooks/", "scripts/")
LANE_ID_PATTERN = re.compile(r"^[a-z0-9][a-z0-9-]*$")


class ContractError(RuntimeError):
    """Raised when the validation contract is internally inconsistent."""


def load_manifest(path: Path = MANIFEST_PATH) -> dict[str, Any]:
    """Load the validation manifest and reject non-object roots."""
    with path.open("rb") as handle:
        manifest = json.load(handle)
    if not isinstance(manifest, dict):
        raise ContractError("validation manifest root must be an object")
    return manifest


def lanes_by_id(manifest: dict[str, Any]) -> dict[str, dict[str, Any]]:
    """Index lanes by identifier, rejecting duplicates and malformed entries."""
    lanes = manifest.get("lanes")
    if not isinstance(lanes, list) or not lanes:
        raise ContractError("validation manifest must contain a non-empty lanes array")
    indexed: dict[str, dict[str, Any]] = {}
    for lane in lanes:
        if not isinstance(lane, dict) or not isinstance(lane.get("id"), str):
            raise ContractError("every validation lane must have a string id")
        lane_id = lane["id"]
        if not LANE_ID_PATTERN.fullmatch(lane_id):
            raise ContractError(f"validation lane has unsafe id: {lane_id}")
        if lane_id in indexed:
            raise ContractError(f"duplicate validation lane: {lane_id}")
        commands = lane.get("commands")
        if not isinstance(commands, list) or not commands:
            raise ContractError(f"lane {lane_id} must contain commands")
        for command in commands:
            if not isinstance(command, list) or not command or not all(
                isinstance(argument, str) and argument for argument in command
            ):
                raise ContractError(f"lane {lane_id} commands must be non-empty string arrays")
        budget = lane.get("budget_seconds")
        if not isinstance(budget, int) or budget <= 0:
            raise ContractError(f"lane {lane_id} must have a positive integer budget_seconds")
        category = lane.get("category")
        if category not in ALLOWED_CATEGORIES:
            raise ContractError(f"lane {lane_id} has unknown category: {category}")
        if "required_pr" in lane and not isinstance(lane["required_pr"], bool):
            raise ContractError(f"lane {lane_id} required_pr must be boolean")
        environment = lane.get("environment", {})
        if not isinstance(environment, dict) or not all(
            isinstance(key, str) and isinstance(value, str)
            for key, value in environment.items()
        ):
            raise ContractError(f"lane {lane_id} environment must contain string pairs")
        indexed[lane_id] = lane
    return indexed


@lru_cache(maxsize=4)
def workspace_metadata(root: Path = ROOT) -> dict[str, Any]:
    """Load Cargo workspace metadata without resolving or downloading dependencies."""
    output = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return json.loads(output)


def workspace_packages(root: Path = ROOT) -> dict[str, Path]:
    """Return local workspace package names and their relative directories."""
    packages: dict[str, Path] = {}
    metadata = workspace_metadata(root)
    members = set(metadata["workspace_members"])
    for package in metadata["packages"]:
        if package["id"] not in members:
            continue
        package_name = package["name"]
        if package_name in packages:
            raise ContractError(f"duplicate workspace package name: {package_name}")
        packages[package_name] = Path(package["manifest_path"]).parent.relative_to(root)
    return packages


def validate_contract(manifest: dict[str, Any], root: Path = ROOT) -> None:
    """Validate schema, package partitioning, categories, and CI lane references."""
    if manifest.get("schema_version") != 1:
        raise ContractError("unsupported validation manifest schema_version")
    workers = manifest.get("max_ci_workers")
    if not isinstance(workers, int) or workers < 1 or workers > 8:
        raise ContractError("max_ci_workers must be between 1 and 8")

    indexed = lanes_by_id(manifest)
    required_categories = ALLOWED_CATEGORIES - {"local-full"}
    present_categories = {lane.get("category") for lane in indexed.values()}
    missing_categories = sorted(required_categories - present_categories)
    if missing_categories:
        raise ContractError(f"missing validation categories: {', '.join(missing_categories)}")

    expected_packages = set(workspace_packages(root))
    assigned_packages: dict[str, str] = {}
    for lane in indexed.values():
        if lane.get("category") != ORDINARY_CATEGORY:
            continue
        for package in lane.get("packages", []):
            if package in assigned_packages:
                raise ContractError(
                    f"ordinary package {package} appears in both "
                    f"{assigned_packages[package]} and {lane['id']}"
                )
            assigned_packages[package] = lane["id"]
    missing_packages = sorted(expected_packages - set(assigned_packages))
    unknown_packages = sorted(set(assigned_packages) - expected_packages)
    if missing_packages or unknown_packages:
        raise ContractError(
            f"ordinary package partition mismatch; missing={missing_packages}, "
            f"unknown={unknown_packages}"
        )

    workflow = (root / ".github" / "workflows" / "ci.yml").read_text()
    validate_workflow_contract(manifest, workflow)


def validate_workflow_contract(manifest: dict[str, Any], workflow: str) -> None:
    """Prove that CI dispatches every required lane exactly once."""
    indexed = lanes_by_id(manifest)
    required = {lane_id for lane_id, lane in indexed.items() if lane.get("required_pr")}
    dispatched: set[str] = set()

    for category in MATRIX_CATEGORIES:
        marker = f"list --category {category} --required-pr"
        if workflow.count(marker) != 1:
            raise ContractError(
                f"ci.yml must generate exactly one required matrix for category {category}"
            )
        dispatched.update(
            lane_id
            for lane_id, lane in indexed.items()
            if lane.get("required_pr") and lane.get("category") == category
        )

    direct_ids: list[str] = []
    marker = "scripts/validation.py run "
    for line in workflow.splitlines():
        if marker not in line:
            continue
        lane_id = line.split(marker, 1)[1].strip().strip('"').strip("'")
        if lane_id.startswith("${{"):
            continue
        if lane_id not in indexed:
            raise ContractError(f"ci.yml references unknown validation lane {lane_id}")
        direct_ids.append(lane_id)
    duplicates = sorted({lane_id for lane_id in direct_ids if direct_ids.count(lane_id) > 1})
    if duplicates:
        raise ContractError(f"ci.yml dispatches lanes more than once: {duplicates}")
    dispatched.update(lane_id for lane_id in direct_ids if indexed[lane_id].get("required_pr"))

    missing = sorted(required - dispatched)
    extra = sorted(dispatched - required)
    if missing or extra:
        raise ContractError(
            f"CI required-lane mismatch; missing={missing}, non_required={extra}"
        )

    workers = manifest["max_ci_workers"]
    if workflow.count(f"max-parallel: {workers}") != len(MATRIX_CATEGORIES):
        raise ContractError(
            "ci.yml matrix worker caps do not match max_ci_workers for every matrix category"
        )


def write_json(path: Path, value: Any) -> None:
    """Atomically write stable JSON."""
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def run_lane(lane: dict[str, Any], report_dir: Path, root: Path = ROOT) -> int:
    """Run one lane, recording outcome and budget status even when it fails."""
    lane_id = lane["id"]
    environment = os.environ.copy()
    environment.update(lane.get("environment", {}))
    started = time.time()
    started_monotonic = time.monotonic()
    commands: list[dict[str, Any]] = []
    outcome = "passed"
    exit_code = 0

    for arguments in lane["commands"]:
        command_started = time.monotonic()
        print(f"[{lane_id}] {subprocess.list2cmdline(arguments)}", flush=True)
        completed = subprocess.run(arguments, cwd=root, env=environment, check=False)
        duration = time.monotonic() - command_started
        commands.append(
            {
                "arguments": arguments,
                "duration_seconds": round(duration, 3),
                "exit_code": completed.returncode,
            }
        )
        if completed.returncode != 0:
            outcome = "failed"
            exit_code = completed.returncode
            break

    duration = time.monotonic() - started_monotonic
    budget = lane["budget_seconds"]
    report = {
        "schema_version": 1,
        "lane": lane_id,
        "category": lane["category"],
        "started_at_unix_seconds": round(started, 3),
        "duration_seconds": round(duration, 3),
        "budget_seconds": budget,
        "budget_status": "within" if duration <= budget else "exceeded",
        "outcome": outcome,
        "commands": commands,
    }
    write_json(report_dir / f"{lane_id}.json", report)
    print(
        f"[{lane_id}] {outcome} in {duration:.1f}s "
        f"(budget {budget}s: {report['budget_status']})",
        flush=True,
    )
    return exit_code


def changed_paths(base: str, root: Path = ROOT) -> list[str]:
    """Return committed, staged, unstaged, and untracked paths since base."""
    merge_base = subprocess.run(
        ["git", "merge-base", base, "HEAD"],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    output = subprocess.run(
        ["git", "diff", "--name-only", "--diff-filter=ACMRD", merge_base],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    untracked = subprocess.run(
        ["git", "ls-files", "--others", "--exclude-standard"],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return sorted({path for path in (output + untracked).splitlines() if path})


def local_dependency_graph(root: Path = ROOT) -> tuple[dict[str, Path], dict[str, set[str]]]:
    """Build a conservative reverse dependency graph from workspace manifests."""
    packages = workspace_packages(root)
    reverse = {package: set() for package in packages}
    metadata = workspace_metadata(root)
    members = set(metadata["workspace_members"])
    for package in metadata["packages"]:
        if package["id"] not in members:
            continue
        dependent = package["name"]
        for dependency in package["dependencies"]:
            dependency_name = dependency["name"]
            if dependency_name in reverse:
                reverse[dependency_name].add(dependent)
    return packages, reverse


def affected_packages(paths: list[str], root: Path = ROOT) -> tuple[list[str], str]:
    """Select affected packages, widening to all packages on unsafe classifications."""
    packages, reverse = local_dependency_graph(root)
    all_packages = sorted(packages)
    selected: set[str] = set()
    roots_by_depth = sorted(packages.items(), key=lambda item: len(item[1].parts), reverse=True)

    for path_text in paths:
        path = Path(path_text)
        if path_text in WORKSPACE_SENSITIVE or path_text.startswith(
            (".cargo/", ".github/") + CONSERVATIVE_PATH_PREFIXES
        ):
            return all_packages, f"workspace-sensitive change: {path_text}"
        if path.name == "build.rs" or path.suffix in {".toml", ".lock"} and path_text.startswith(SOURCE_PREFIXES):
            return all_packages, f"build-affecting change: {path_text}"
        matched = False
        for package, directory in roots_by_depth:
            try:
                path.relative_to(directory)
            except ValueError:
                continue
            selected.add(package)
            matched = True
            break
        if not matched and (path_text.startswith(SOURCE_PREFIXES) or path.suffix == ".rs"):
            return all_packages, f"unclassified source change: {path_text}"

    queue = list(selected)
    while queue:
        dependency = queue.pop()
        for dependent in reverse[dependency]:
            if dependent not in selected:
                selected.add(dependent)
                queue.append(dependent)
    return sorted(selected), "package paths plus reverse local dependencies"


def run_affected(base: str, report_dir: Path, root: Path = ROOT) -> int:
    """Run tests for conservatively selected affected workspace packages."""
    paths = changed_paths(base, root)
    packages, reason = affected_packages(paths, root)
    if not packages:
        print(f"No Rust workspace package tests selected ({reason}).")
        return 0
    lane = {
        "id": "affected",
        "category": ORDINARY_CATEGORY,
        "budget_seconds": 1200,
        "commands": [
            ["cargo", "test"]
            + [argument for package in packages for argument in ("-p", package)]
            + ["--", "--skip", "dst_"]
        ],
    }
    print(f"Affected selection: {', '.join(packages)} ({reason})")
    return run_lane(lane, report_dir, root)


def summarize(manifest: dict[str, Any], report_dir: Path, require_complete: bool) -> int:
    """Combine lane reports and emit JSON plus a Markdown summary."""
    required = {
        lane["id"] for lane in lanes_by_id(manifest).values() if lane.get("required_pr")
    }
    reports = []
    seen_lanes: set[str] = set()
    for path in sorted(report_dir.rglob("*.json")):
        if path.name == "summary.json":
            continue
        with path.open("rb") as handle:
            report = json.load(handle)
        if isinstance(report, dict) and "lane" in report:
            if require_complete and report["lane"] not in required:
                continue
            if report["lane"] in seen_lanes:
                raise ContractError(f"duplicate validation report for lane {report['lane']}")
            seen_lanes.add(report["lane"])
            reports.append(report)
    missing = []
    if require_complete:
        missing = sorted(required - seen_lanes)
    summary = {
        "schema_version": 1,
        "lanes": reports,
        "failed": sum(report["outcome"] != "passed" for report in reports),
        "budget_exceeded": sum(report["budget_status"] == "exceeded" for report in reports),
        "total_lane_seconds": round(sum(report["duration_seconds"] for report in reports), 3),
        "missing_required_lanes": missing,
    }
    write_json(report_dir / "summary.json", summary)
    print("| Lane | Outcome | Duration | Budget | Budget status |")
    print("|---|---:|---:|---:|---:|")
    for report in reports:
        print(
            f"| {report['lane']} | {report['outcome']} | "
            f"{report['duration_seconds']:.1f}s | {report['budget_seconds']}s | "
            f"{report['budget_status']} |"
        )
    if missing:
        print(f"Missing required lane reports: {', '.join(missing)}")
    return 1 if summary["failed"] or missing else 0


def list_lanes(manifest: dict[str, Any], category: str | None, required_pr: bool) -> None:
    """Print a GitHub Actions-compatible matrix for selected lanes."""
    lanes = []
    for lane in lanes_by_id(manifest).values():
        if category is not None and lane["category"] != category:
            continue
        if required_pr and not lane.get("required_pr"):
            continue
        lanes.append({"lane": lane["id"]})
    print(json.dumps({"include": lanes}, separators=(",", ":")))


def parse_args() -> argparse.Namespace:
    """Parse command-line arguments."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=MANIFEST_PATH)
    parser.add_argument("--report-dir", type=Path, default=ROOT / "target" / "validation-reports")
    subparsers = parser.add_subparsers(dest="operation", required=True)
    subparsers.add_parser("check")
    list_parser = subparsers.add_parser("list")
    list_parser.add_argument("--category")
    list_parser.add_argument("--required-pr", action="store_true")
    run_parser = subparsers.add_parser("run")
    run_parser.add_argument("lane")
    affected_parser = subparsers.add_parser("affected")
    affected_parser.add_argument("--base", default="fork/main")
    mode_parser = subparsers.add_parser("mode")
    mode_parser.add_argument("mode", choices=("fast", "affected", "backend-parity", "full"))
    mode_parser.add_argument("--base", default="fork/main")
    summary_parser = subparsers.add_parser("summary")
    summary_parser.add_argument("--require-complete", action="store_true")
    return parser.parse_args()


def main() -> int:
    """Run the requested validation operation."""
    args = parse_args()
    manifest = load_manifest(args.manifest)
    indexed = lanes_by_id(manifest)
    if args.operation == "check":
        validate_contract(manifest)
        print(f"Validation contract OK: {len(indexed)} lanes")
        return 0
    if args.operation == "list":
        list_lanes(manifest, args.category, args.required_pr)
        return 0
    if args.operation == "run":
        if args.lane not in indexed:
            raise ContractError(f"unknown validation lane: {args.lane}")
        return run_lane(indexed[args.lane], args.report_dir)
    if args.operation == "affected":
        return run_affected(args.base, args.report_dir)
    if args.operation == "summary":
        return summarize(manifest, args.report_dir, args.require_complete)
    if args.mode == "affected":
        return run_affected(args.base, args.report_dir)
    if args.mode == "backend-parity":
        return run_lane(indexed["backend-parity"], args.report_dir)
    if args.mode == "fast":
        for lane_id in FAST_LANES:
            exit_code = run_lane(indexed[lane_id], args.report_dir)
            if exit_code:
                return exit_code
        return run_affected(args.base, args.report_dir)
    for lane in indexed.values():
        if lane.get("required_pr"):
            exit_code = run_lane(lane, args.report_dir)
            if exit_code:
                return exit_code
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ContractError, OSError, subprocess.CalledProcessError) as error:
        print(f"validation error: {error}", file=sys.stderr)
        raise SystemExit(2) from error
