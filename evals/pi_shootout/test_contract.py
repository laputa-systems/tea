from __future__ import annotations

import copy
import unittest

from .contract import ContractError, RESULT_SCHEMA, validate_result
from .runner import Config, DEFAULT_MODEL, DEFAULT_THINKING, ShootoutError, randomized_plan


def result(*, baseline: str = "tea-jit") -> dict:
    return {
        "schema_version": RESULT_SCHEMA,
        "attempt_id": "attempt-1",
        "baseline_id": baseline,
        "terminal": {"status": "completed", "code": None},
        "final_text": "done",
        "runtime": {"implementation": "tea", "version": "1.0.0", "revision": "abc", "dirty": False, "dirty_digest": None},
        "model": {"provider": "openrouter", "requested_model": DEFAULT_MODEL, "returned_model": None, "returned_provider": None, "thinking_level": DEFAULT_THINKING, "max_output_tokens": None, "sampling": {"temperature": None, "seed": None, "source": "provider-default"}},
        "surface": {"system_prompt_bytes": 1, "system_prompt_sha256": "a", "workspace_normalized_system_prompt_sha256": "b", "tool_surface_sha256": "c", "active_tools": ["read", "bash", "edit", "write"], "research_tools": [], "subagents": False, "shell_curl_available": True, "shell_environment_sha256": "d"},
        "timings": {"agent_ms": 1, "candidate_validation_ms": 0, "rollover_ms": 0},
        "counts": {"turns": 1, "provider_requests": None, "tool_calls": 0, "retries": 0, "compactions": 0},
        "usage": {"input": 2, "output": 3, "generation": 5, "reasoning": None, "cache_read": 0, "cache_write": 0},
        "cost": {"kind": "unavailable", "currency": "USD", "total": None},
        "harness": {"mode": "jit", "base_snapshot_id": "base", "initial_snapshot_id": "base", "final_snapshot_id": "base", "decision": "no-change", "candidate_count": 0, "candidate_id": None, "changed_surfaces": [], "candidate_source_bytes": 0, "hypothesis": None},
        "trace": [],
    }


class ContractTest(unittest.TestCase):
    def test_accepts_unbounded_output_and_generation_identity(self) -> None:
        self.assertEqual(validate_result(result())["usage"]["generation"], 5)

    def test_rejects_incorrect_generation_total(self) -> None:
        value = result()
        value["usage"]["generation"] = 6
        with self.assertRaises(ContractError):
            validate_result(value)

    def test_accepts_empty_final_text_for_a_terminal_model_failure(self) -> None:
        value = result()
        value["terminal"] = {"status": "failed", "code": "provider_error"}
        value["final_text"] = ""
        self.assertEqual(validate_result(value)["terminal"]["status"], "failed")

    def test_rejects_static_harness_decision(self) -> None:
        value = result(baseline="tea-static")
        value["harness"]["mode"] = "static"
        with self.assertRaises(ContractError):
            validate_result(value)

    def test_plan_randomization_is_seeded_and_has_every_condition(self) -> None:
        first, second = randomized_plan(3, 20260823), randomized_plan(3, 20260823)
        self.assertEqual(first, second)
        self.assertTrue(all(set(order) == {"pi-static", "tea-static", "tea-jit"} for order in first))

    def test_configuration_requires_the_fixed_v0_model_and_unbounded_is_valid(self) -> None:
        config = Config("express-3936-medium", "openrouter", DEFAULT_MODEL, DEFAULT_THINKING, None, 1, 1, __import__("pathlib").Path("/tmp/cache"), __import__("pathlib").Path("/tmp/work"), __import__("pathlib").Path("/tmp/out"))
        config.validate()
        with self.assertRaises(ShootoutError):
            Config("express-3936-medium", "openrouter", "poolside/laguna-xs-2.1:free", DEFAULT_THINKING, None, 1, 1, config.cache_root, config.workspace_root, config.out).validate()
        with self.assertRaises(ShootoutError):
            Config("express-3936-medium", "openrouter", DEFAULT_MODEL, DEFAULT_THINKING, 4096, 1, 1, config.cache_root, config.workspace_root, config.out).validate()


if __name__ == "__main__":
    unittest.main()
