from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from .report import evolution_report, static_report, write_reports
from .test_contract import result


def record(baseline: str, *, passed: bool, decision: str = "not-applicable") -> dict:
    value = result(baseline=baseline)
    if baseline != "tea-jit":
        value["harness"] = {"mode": "static", "base_snapshot_id": "base", "initial_snapshot_id": "base", "final_snapshot_id": "base", "decision": "not-applicable", "candidate_count": 0, "candidate_id": None, "changed_surfaces": [], "candidate_source_bytes": 0, "hypothesis": None}
    else:
        value["harness"]["decision"] = decision
    return {"baseline_id": baseline, "adapter_result": value, "validator": {"passed": passed}, "timings": {"total_attempt_ms": 2}, "process": {"peak_rss_bytes": None}, "patch_sha256": baseline}


def summary(*, pi: bool = False, static: bool = False, jit: bool = False, decision: str = "no-change") -> dict:
    return {"run": {"run_id": "run", "task_id": "express-3936-medium", "task_manifest_sha256": "task", "baseline_commit": "base", "validator_sha256": "validator", "model": "deepseek/deepseek-v4-flash-0731", "provider": "openrouter", "thinking_level": "high", "max_output_tokens": None, "timeout_seconds": 900, "condition_order": ["pi-static", "tea-static", "tea-jit"]}, "attempts": [record("pi-static", passed=pi), record("tea-static", passed=static), record("tea-jit", passed=jit, decision=decision)]}


class ReportTest(unittest.TestCase):
    def test_static_report_has_surface_parity_and_results(self) -> None:
        text = static_report(summary())
        self.assertIn("Harness-surface parity", text)
        self.assertIn("Generation tokens", text)

    def test_evolution_report_classifies_positive_flip(self) -> None:
        self.assertIn("positive flip", evolution_report(summary(jit=True, decision="activated")))

    def test_evolution_report_classifies_regression_and_no_change(self) -> None:
        self.assertIn("regression", evolution_report(summary(static=True, decision="activated")))
        self.assertIn("no-change", evolution_report(summary(decision="no-change")))

    def test_reports_mark_terminal_failures_non_comparable(self) -> None:
        value = summary()
        value["attempts"][0]["adapter_result"]["terminal"] = {"status": "failed", "code": "provider_429"}
        self.assertIn("Not comparable as an efficacy result", static_report(value))

    def test_reports_are_persisted_separately(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            static, evolution, surface = write_reports(summary(), Path(temporary))
            self.assertTrue(static.is_file())
            self.assertTrue(evolution.is_file())
            self.assertTrue(surface.is_file())


if __name__ == "__main__":
    unittest.main()
