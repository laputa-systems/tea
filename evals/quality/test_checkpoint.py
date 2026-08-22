"""Regression coverage for the experimental structured checkpoint candidate."""

from __future__ import annotations

import unittest

from .checkpoint import LedgerEntry, StructuredCheckpoint, parse_checkpoint


class StructuredCheckpointTest(unittest.TestCase):
    def test_five_successive_merges_preserve_marker_and_latest_state(self) -> None:
        checkpoint = StructuredCheckpoint.empty()
        for generation in range(1, 6):
            checkpoint = checkpoint.merge(
                {
                    "Goal": ["finish compaction"],
                    "Current Checkpoint": [f"generation {generation}"],
                    "Verification": [f"test-{generation} passed"],
                },
                [LedgerEntry("test", f"test-{generation}", "/repo", "passed", generation=generation)],
            )
        rendered = checkpoint.render()
        parsed = parse_checkpoint(rendered)
        self.assertIsNotNone(parsed)
        assert parsed is not None
        self.assertEqual(parsed.generation, 5)
        self.assertEqual(parsed.sections["Current Checkpoint"], ["generation 5"])
        self.assertEqual(len(checkpoint.ledger.entries), 5)

    def test_ledger_deduplicates_only_same_observed_operation(self) -> None:
        ledger = StructuredCheckpoint.empty().ledger.merge(
            [
                LedgerEntry("read", "src/a.rs", "/repo", "passed", generation=1),
                LedgerEntry("read", "src/a.rs", "/repo", "passed", generation=2),
                LedgerEntry("modify", "src/a.rs", "/repo", "passed", generation=2),
            ]
        )
        self.assertEqual(len(ledger.entries), 2)
        self.assertEqual(ledger.entries[0].generation, 2)

    def test_unmarked_text_is_not_a_checkpoint(self) -> None:
        self.assertIsNone(parse_checkpoint("## Goal\n- ordinary markdown"))


if __name__ == "__main__":
    unittest.main()
