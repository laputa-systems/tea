"""Provider-free compaction quality-contract tests."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from .compaction import SCENARIOS, run_compaction_quality


class CompactionQualityTest(unittest.TestCase):
    def test_matrix_has_a_stable_broad_contract(self) -> None:
        self.assertEqual(len(SCENARIOS), 70)
        self.assertEqual(len({scenario["id"] for scenario in SCENARIOS}), len(SCENARIOS))

    def test_provider_free_runner_writes_one_report_per_scenario(self) -> None:
        with tempfile.TemporaryDirectory(prefix="tea-compaction-quality-") as temporary:
            out = Path(temporary)
            status, summary = run_compaction_quality(out=out)
            self.assertEqual(status, 0, summary)
            self.assertEqual(summary["passed"], len(SCENARIOS))
            self.assertTrue((out / "summary.json").is_file())
            self.assertTrue((out / f"{SCENARIOS[0]['id']}.json").is_file())


if __name__ == "__main__":
    unittest.main()
