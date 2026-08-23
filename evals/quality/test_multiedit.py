"""Provider-free checks for the disabled-tool multiedit evaluation scaffold."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from .multiedit import (
    SCHEMA,
    TRUSTED_RUNNER_ID,
    grade_verified_record,
    hidden_case,
    hidden_case_digest,
    public_task,
    write_multiedit_task,
)


class MultiEditQualityTest(unittest.TestCase):
    def test_public_task_never_exposes_the_disabled_capability_or_grader(self) -> None:
        task = public_task()
        self.assertEqual(task["capabilities"], ["read", "bash", "edit", "write"])
        self.assertEqual(task["disabled_capabilities"], ["multiedit"])
        self.assertNotIn("files[]", task["task"])
        with tempfile.TemporaryDirectory(prefix="tea-multiedit-quality-") as temporary:
            path = write_multiedit_task(Path(temporary))
            self.assertEqual(json.loads(path.read_text(encoding="utf-8"))["schema_version"], SCHEMA)
            self.assertFalse((Path(temporary) / "task" / "hidden.json").exists())

    def test_public_task_refuses_a_directory_with_unexpected_material(self) -> None:
        with tempfile.TemporaryDirectory(prefix="tea-multiedit-quality-") as temporary:
            task = Path(temporary) / "task"
            task.mkdir()
            (task / "hidden.json").write_text("{}", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "unexpected material"):
                write_multiedit_task(Path(temporary))

    def test_untrusted_or_wrong_case_record_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory(prefix="tea-multiedit-quality-") as temporary:
            record = _record(Path(temporary))
            record["trusted_grader"]["case_digest"] = "not-the-hidden-case"
            with self.assertRaisesRegex(ValueError, "trusted hidden grader"):
                grade_verified_record(record)

    def test_semantic_failure_cannot_trade_for_design_or_efficiency(self) -> None:
        with tempfile.TemporaryDirectory(prefix="tea-multiedit-quality-") as temporary:
            root = Path(temporary)
            record = _record(root)
            record["validation"]["probes"]["workspace_escape"]["unchanged"] = False
            graded = grade_verified_record(record)
            self.assertFalse(graded["passed"])
            self.assertEqual(graded["total_score"], 0)

    def test_efficiency_cannot_trade_for_an_unproved_design(self) -> None:
        with tempfile.TemporaryDirectory(prefix="tea-multiedit-quality-") as temporary:
            record = _record(Path(temporary))
            record["design_rubric"] = {"contract": 5, "proof": 0, "limitations": 5}
            graded = grade_verified_record(record)
            self.assertFalse(graded["passed"])
            self.assertIn("design_proof_threshold", graded["failed_checks"])
            self.assertEqual(graded["efficiency_score"], 15)

    def test_verified_success_uses_the_70_15_15_rubric_and_full_vector(self) -> None:
        with tempfile.TemporaryDirectory(prefix="tea-multiedit-quality-") as temporary:
            graded = grade_verified_record(_record(Path(temporary)))
            self.assertTrue(graded["passed"])
            self.assertEqual(graded["semantic_score"], 70)
            self.assertEqual(graded["design_proof_score"], 15)
            self.assertEqual(graded["efficiency_score"], 15)
            self.assertEqual(graded["total_score"], 100)
            self.assertEqual(
                set(graded["efficiency_components"]),
                {"tool_calls", "turns", "wall_clock_ms", "output_tokens", "remote_round_trips", "context_bytes"},
            )


def _record(root: Path) -> dict[str, object]:
    (root / "lib").mkdir(parents=True, exist_ok=True)
    (root / "lib" / "alpha.txt").write_text("after alpha\n", encoding="utf-8")
    (root / "lib" / "beta.txt").write_text("after beta\n", encoding="utf-8")
    case = hidden_case()
    no_partial = {"outcome": "rejected", "unchanged": True}
    return {
        "schema_version": SCHEMA,
        "capabilities": ["read", "bash", "edit", "write"],
        "trusted_grader": {
            "runner_id": TRUSTED_RUNNER_ID,
            "case_id": case["id"],
            "case_digest": hidden_case_digest(case),
        },
        "validation": {
            "workspace": str(root),
            "probes": {
                "stale": dict(no_partial),
                "overlap_duplicate": dict(no_partial),
                "workspace_escape": dict(no_partial),
                "non_regular": dict(no_partial),
                "fault": {"outcome": "rolled_back", "all_targets_inspected": True},
                "cancel_before_commit": {"outcome": "cancelled", "unchanged": True},
                "cancel_after_commit": {"receipt": "committed", "settled_before_agent_end": True},
            },
        },
        "design_rubric": {"contract": 5, "proof": 5, "limitations": 5},
        "efficiency": {
            "tool_calls": 8,
            "turns": 2,
            "wall_clock_ms": 3000,
            "output_tokens": 1500,
            "remote_round_trips": 3,
            "context_bytes": 64000,
        },
    }
