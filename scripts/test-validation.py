#!/usr/bin/env python3
"""Unit tests for the validation contract and affected-package classifier."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("validation.py")
SPEC = importlib.util.spec_from_file_location("temper_validation", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
validation = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(validation)


class ValidationContractTests(unittest.TestCase):
    def test_repository_contract_is_complete(self) -> None:
        manifest = validation.load_manifest()
        validation.validate_contract(manifest)

    def test_duplicate_lane_is_rejected(self) -> None:
        manifest = validation.load_manifest()
        manifest["lanes"].append(dict(manifest["lanes"][0]))
        with self.assertRaisesRegex(validation.ContractError, "duplicate validation lane"):
            validation.lanes_by_id(manifest)

    def test_unsafe_lane_id_is_rejected(self) -> None:
        manifest = validation.load_manifest()
        manifest["lanes"][0]["id"] = "../outside"
        with self.assertRaisesRegex(validation.ContractError, "unsafe id"):
            validation.lanes_by_id(manifest)

    def test_unknown_category_is_rejected(self) -> None:
        manifest = validation.load_manifest()
        manifest["lanes"][0]["category"] = "almost-format"
        with self.assertRaisesRegex(validation.ContractError, "unknown category"):
            validation.lanes_by_id(manifest)

    def test_unknown_ci_lane_is_rejected(self) -> None:
        manifest = validation.load_manifest()
        workflow = validation.ROOT.joinpath(".github/workflows/ci.yml").read_text()
        workflow += "\nrun: python3 scripts/validation.py run not-a-lane\n"
        with self.assertRaisesRegex(validation.ContractError, "unknown validation lane"):
            validation.validate_workflow_contract(manifest, workflow)

    def test_duplicate_ci_lane_is_rejected(self) -> None:
        manifest = validation.load_manifest()
        workflow = validation.ROOT.joinpath(".github/workflows/ci.yml").read_text()
        workflow += "\nrun: python3 scripts/validation.py run fmt\n"
        with self.assertRaisesRegex(validation.ContractError, "more than once"):
            validation.validate_workflow_contract(manifest, workflow)

    def test_workspace_change_selects_every_package(self) -> None:
        selected, reason = validation.affected_packages(["Cargo.lock"])
        self.assertEqual(selected, sorted(validation.workspace_packages()))
        self.assertIn("workspace-sensitive", reason)

    def test_validation_script_change_selects_every_package(self) -> None:
        selected, reason = validation.affected_packages(["scripts/validation.py"])
        self.assertEqual(selected, sorted(validation.workspace_packages()))
        self.assertIn("workspace-sensitive", reason)

    def test_package_change_includes_reverse_dependencies(self) -> None:
        selected, _ = validation.affected_packages(["crates/temper-runtime/src/lib.rs"])
        self.assertIn("temper-runtime", selected)
        self.assertIn("temper-server", selected)

    def test_unclassified_source_change_selects_every_package(self) -> None:
        selected, reason = validation.affected_packages(["os-apps/new-runtime/file.rs"])
        self.assertEqual(selected, sorted(validation.workspace_packages()))
        self.assertIn("unclassified source", reason)

    def test_lane_failure_still_writes_report(self) -> None:
        lane = {
            "id": "expected-failure",
            "category": "test",
            "budget_seconds": 10,
            "commands": [["sh", "-c", "exit 7"]],
        }
        with tempfile.TemporaryDirectory() as temporary:
            report_dir = Path(temporary)
            self.assertEqual(validation.run_lane(lane, report_dir), 7)
            report = json.loads((report_dir / "expected-failure.json").read_text())
        self.assertEqual(report["outcome"], "failed")
        self.assertEqual(report["commands"][0]["exit_code"], 7)

    def test_required_summary_ignores_non_contract_reports(self) -> None:
        manifest = {
            "lanes": [
                {
                    "id": "required",
                    "category": "format",
                    "budget_seconds": 10,
                    "required_pr": True,
                    "commands": [["true"]],
                }
            ]
        }
        with tempfile.TemporaryDirectory() as temporary:
            report_dir = Path(temporary)
            for lane, outcome in (("required", "passed"), ("affected", "failed")):
                (report_dir / f"{lane}.json").write_text(
                    json.dumps(
                        {
                            "lane": lane,
                            "outcome": outcome,
                            "duration_seconds": 1,
                            "budget_seconds": 10,
                            "budget_status": "within",
                        }
                    )
                )
            self.assertEqual(validation.summarize(manifest, report_dir, True), 0)
            summary = json.loads((report_dir / "summary.json").read_text())
        self.assertEqual([report["lane"] for report in summary["lanes"]], ["required"])


if __name__ == "__main__":
    unittest.main()
