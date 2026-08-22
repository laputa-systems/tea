"""Regression checks for deterministic continuation fixture evidence."""

import unittest

from .continuation import run_continuation_fixtures


class ContinuationFixtureTests(unittest.TestCase):
    def test_corpus_has_five_passing_provider_free_episodes(self) -> None:
        report = run_continuation_fixtures()
        self.assertEqual(report["case_count"], 5)
        self.assertEqual(report["passed"], 5)
        self.assertEqual(report["failed"], [])
        self.assertIn("not provider-generated", report["evidence"])

    def test_rework_fixture_distinguishes_unchanged_duplicate_from_retry_after_edit(self) -> None:
        report = run_continuation_fixtures()
        case = next(item for item in report["cases"] if item["id"] == "operation-ledger-rework-classification")
        metrics = case["metrics"]
        self.assertEqual(metrics["repeated_unchanged_read_bytes"], 100)
        self.assertEqual(metrics["repeated_failed_approaches"], 1)
        # The post-edit test retry has a new workspace generation and is not a duplicate.
        self.assertEqual(metrics["duplicate_tool_calls"], 2)

