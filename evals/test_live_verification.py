"""Provider-free checks for the guarded live-verification entry surface."""

from __future__ import annotations

import json
from datetime import date
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

    def test_model_record_is_exact_and_sanitized(self) -> None:
        evidence = json.loads((ROOT / "evals" / "live" / "codex-luna-model-evidence.json").read_text(encoding="utf-8"))
        self.assertEqual(evidence["provider"], "codex")
        self.assertEqual(evidence["model"], "gpt-5.6-luna")
        self.assertEqual(evidence["endpoint"], "https://chatgpt.com/backend-api/codex/responses")
        self.assertEqual(evidence["reasoning_effort"], "low")
        self.assertEqual(evidence["model_source"], "https://learn.chatgpt.com/docs/models")
        self.assertNotIn("credential", evidence)
        self.assertEqual(date.fromisoformat(evidence["checked_on"]).isoformat(), evidence["checked_on"])
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
