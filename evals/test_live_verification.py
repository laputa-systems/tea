"""Provider-free checks for the guarded live-verification entry surface."""

from __future__ import annotations

import json
from pathlib import Path
import unittest

from evals.quality import __main__ as quality_cli
from evals.quality.coding_runner import CodingRunError, run_coding_cases


ROOT = Path(__file__).resolve().parent.parent


class LiveVerificationEntrypointTests(unittest.TestCase):
    def test_quality_cli_has_no_live_model_command(self) -> None:
        choices = next(action.choices for action in quality_cli.parser()._actions if action.dest == "command")
        self.assertNotIn("coding", choices)
        self.assertNotIn("full", choices)

    def test_catalog_record_is_exact_and_sanitized(self) -> None:
        evidence = json.loads((ROOT / "evals" / "live" / "zen-free-catalog-evidence.json").read_text(encoding="utf-8"))
        self.assertEqual(evidence["provider"], "opencode-zen")
        self.assertEqual(evidence["model"], "muse-spark-1.3-contributor-free")
        self.assertEqual(evidence["endpoint"], "https://opencode.ai/zen/v1/responses")
        self.assertEqual(
            evidence["pricing_per_million"],
            {"cached_read": "Free", "cached_write": None, "input": "Free", "output": "Free"},
        )
        self.assertEqual(evidence["checked_on"], "2026-09-22")
        self.assertTrue(evidence["synthetic_or_public_fixture_only"])

    def test_retired_shell_runner_cannot_start_cargo(self) -> None:
        source = (ROOT / "evals" / "run-rust-live.sh").read_text(encoding="utf-8")
        self.assertIn("No provider request was made.", source)
        self.assertNotIn("exec cargo run", source)

    def test_historical_python_runner_refuses_before_provider_setup(self) -> None:
        with self.assertRaises(CodingRunError):
            run_coding_cases(
                model="arbitrary/model",
                cache_root=Path("/missing-cache"),
                workspace_root=Path("/missing-workspaces"),
                out=Path("/missing-out"),
                validator="fast",
                env_file=Path("/missing-env"),
            )

    def test_python_runner_retains_no_transport_implementation(self) -> None:
        source = (ROOT / "evals" / "quality" / "coding_runner.py").read_text(encoding="utf-8")
        self.assertNotIn("subprocess.", source)
        self.assertNotIn("_retired_run_coding_cases", source)


if __name__ == "__main__":
    unittest.main()
