#!/usr/bin/env python3
"""Smoke-test the Rust quality adapter process boundary without a provider."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[3]
FIXTURE = "crates/tea-core/fixtures/declarative/single-turn-text.json"
QUALITY_FIXTURE = "evals/quality/cases/core/unknown-tool/manifest.json"


class AdapterProtocolTest(unittest.TestCase):
    def run_adapter(self, name: str, fixture: str) -> tuple[int, dict[str, object], str]:
        adapter = ROOT / "evals" / "quality" / "adapters" / name / "adapter.py"
        request = {"protocol": "tea-quality-adapter/v1", "operation": "run", "fixture": fixture}
        environment = dict(os.environ)
        environment["PYTHONDONTWRITEBYTECODE"] = "1"
        completed = subprocess.run(
            [str(adapter)],
            cwd=ROOT,
            input=json.dumps(request),
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=environment,
            check=False,
        )
        self.assertIn(completed.returncode, (0, 1), completed.stderr)
        documents = [line for line in completed.stdout.splitlines() if line.strip()]
        self.assertEqual(len(documents), 1, completed.stdout)
        response = json.loads(documents[0])
        self.assertIsInstance(response, dict)
        return completed.returncode, response, completed.stderr

    def test_rust_direct_declarative_fixture(self) -> None:
        status, response, _ = self.run_adapter("rust-core", FIXTURE)
        self.assertEqual(status, 0)
        self.assertEqual(response["protocol"], "tea-quality-adapter/v1")
        self.assertEqual(response["adapter"], "rust-core")
        self.assertEqual(response["metadata"]["toolchain"], "nightly-2026-07-24")
        self.assertEqual(response["result"]["fixture_id"], "single-turn-text")

    def test_quality_case_is_lowered_before_runner_invocation(self) -> None:
        from evals.quality.suite import compile_core_fixture

        fixture = compile_core_fixture(json.loads((ROOT / QUALITY_FIXTURE).read_text()))
        with tempfile.TemporaryDirectory(prefix="pi-quality-fixture-") as temporary:
            path = Path(temporary) / "fixture.json"
            path.write_text(json.dumps(fixture))
            status, response, _ = self.run_adapter("rust-core", str(path))
        self.assertEqual(status, 0)
        self.assertEqual(response["result"]["fixture_id"], "unknown-tool-continues")


if __name__ == "__main__":
    unittest.main()
