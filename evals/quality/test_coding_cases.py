"""Provider-free contract checks for the small ecological coding suite."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import call, patch

from . import coding_cases
from .coding_cases import CodingCaseError, assert_oracle_isolated_worktree, cache_bare_repository, load_cases
from .coding_runner import _adapter_task, coding_bundle_capabilities


class CodingCasesTest(unittest.TestCase):
    def test_exact_three_requested_cases_and_pins(self) -> None:
        cases = load_cases()
        self.assertEqual(
            [case["id"] for case in cases],
            ["express-3936-medium", "express-4205-hard", "express-4744-easy"],
        )
        for case in cases:
            self.assertEqual(case["setup"]["network"], False)
            self.assertEqual(case["setup"]["tools"], ["read", "bash", "edit", "find"])
            self.assertEqual(case["validators"]["full"]["audit_command"], "npm install && npm test")
            self.assertEqual(case["validators"]["fast"]["evidence"]["baseline"], "fails")
            self.assertEqual(case["validators"]["fast"]["evidence"]["known_correct"], "passes")

    def test_scoring_requires_a_prepopulated_bare_cache(self) -> None:
        case = load_cases()[0]
        with tempfile.TemporaryDirectory(prefix="tea-quality-cache-") as temporary:
            with self.assertRaises(CodingCaseError):
                cache_bare_repository(case["baseline"]["repository"], case["baseline"]["commit"], Path(temporary))

    def test_cache_publishes_a_private_ref_for_each_verified_commit(self) -> None:
        case = load_cases()[0]
        repository = case["baseline"]["repository"]
        commit = case["baseline"]["commit"]
        with tempfile.TemporaryDirectory(prefix="tea-quality-cache-") as temporary:
            root = Path(temporary)
            key = coding_cases.hashlib.sha256(repository.encode()).hexdigest()[:32]
            bare = root / "bare" / f"{key}.git"
            bare.mkdir(parents=True)
            with patch.object(coding_cases, "_git") as git:
                cache_bare_repository(repository, commit, root, populate=True)
            self.assertIn(
                call(
                    "--git-dir",
                    str(bare.resolve()),
                    "update-ref",
                    f"refs/heads/tea-quality/{commit}",
                    commit,
                ),
                git.call_args_list,
            )

    def test_adapter_task_uses_the_default_coding_bundle_tool_contract(self) -> None:
        capabilities = coding_bundle_capabilities()
        task = _adapter_task(load_cases()[0], capabilities)
        self.assertEqual(task["capabilities"], capabilities)
        self.assertEqual([tool["name"] for tool in capabilities], ["read", "bash", "edit", "find"])

    def test_oracle_isolation_rejects_a_worktree_that_contains_the_fix(self) -> None:
        with tempfile.TemporaryDirectory(prefix="tea-quality-worktree-") as temporary:
            workspace = Path(temporary)
            with patch.object(coding_cases, "_git") as git, patch.object(coding_cases.subprocess, "run") as run:
                git.side_effect = [
                    type("Result", (), {"stdout": "baseline\n"})(),
                    type("Result", (), {"stdout": ""})(),
                    type("Result", (), {"stdout": ""})(),
                ]
                run.return_value = type("Result", (), {"returncode": 0})()
                with self.assertRaises(CodingCaseError):
                    assert_oracle_isolated_worktree(workspace, "a" * 40, "b" * 40)


if __name__ == "__main__":
    unittest.main()
