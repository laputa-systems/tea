from __future__ import annotations

import copy
import threading
import unittest
from unittest.mock import Mock, patch

from .contract import ContractError, RESULT_SCHEMA, validate_result
from .runner import Config, DEFAULT_MODEL, DEFAULT_THINKING, DEFAULT_TIMEOUT_SECONDS, HARD_TIMEOUT_SECONDS, ShootoutError, TASK_TIMEOUT_SECONDS, _run_process, plan, randomized_plan, run_repeat_lanes, toolchain_manifest


def result(*, baseline: str = "tea-jit") -> dict:
    request = {
        "ordinal": 1,
        "canonical_request_sha256": "request",
        "model": DEFAULT_MODEL,
        "message_count": 1,
        "message_roles": ["system"],
        "messages": [{"ordinal": 1, "role": "system", "structural_sha256": "message", "content_sha256": "content"}],
        "system_prompt_sha256": "system",

        "assistant_reasoning_content": None,
        "tool_count": 4,
        "tool_names": ["read", "bash", "edit", "find"],
        "tool_schema_sha256": "tools",
        "reasoning": {"effort": "high"},
        "temperature": {"present": False, "value": None}, "seed": {"present": False, "value": None},
        "max_tokens": {"present": False, "value": None}, "max_completion_tokens": {"present": False, "value": None},
        "tool_choice": {"present": False, "value": None}, "parallel_tool_calls": {"present": False, "value": None},
        "stream": {"present": True, "value": True}, "stream_options": {"present": True, "value": {"include_usage": True}},
        "provider_routing": {"require_parameters": True}, "other_model_affecting_top_level_fields": {},
    }
    return {
        "schema_version": RESULT_SCHEMA,
        "attempt_id": "attempt-1",
        "baseline_id": baseline,
        "terminal": {"status": "completed", "code": None},
        "final_text": "done",
        "runtime": {"implementation": "tea", "version": "1.0.0", "revision": "abc", "dirty": False, "dirty_digest": None},
        "model": {"provider": "openrouter", "requested_model": DEFAULT_MODEL, "returned_model": None, "returned_provider": None, "returned_model_provenance": None, "returned_provider_provenance": None, "thinking_level": DEFAULT_THINKING, "max_output_tokens": None, "sampling": {"temperature": None, "seed": None, "source": "provider-default"}},
        "surface": {"system_prompt_bytes": 1, "system_prompt_sha256": "a", "workspace_normalized_system_prompt_sha256": "b", "tool_surface_sha256": "c", "prompt_tool_surface_sha256": "prompt-tools", "wire_tool_surface_sha256": "wire-tools", "execution_surface_sha256": "execution", "active_tools": ["read", "bash", "edit", "find"], "authority": {"tools": ["read", "bash", "edit", "find"], "shell": True, "secret_boundary": "allowlist"}, "research_tools": [], "subagents": False, "shell_curl_available": True, "shell_environment_sha256": "d"},
        "timings": {"agent_ms": 1, "candidate_validation_ms": 0, "rollover_ms": 0},
        "counts": {"turns": 1, "model_turns": 1, "provider_requests": None, "tool_calls": 0, "retries": 0, "compactions": 0},
        "usage": {"input": 2, "prompt_total": 2, "output": 3, "generation": 5, "all_tokens": 5, "reasoning": None, "cache_read": 0, "cache_write": 0},
        "cost": {"kind": "unavailable", "currency": "USD", "total": None},
        "wire": {"source": "direct-final-openrouter-boundary", "request_count": 1, "requests": [request], "routing_policy": {"require_parameters": True}, "returned_route": {"model": None, "provider": None, "provenance": None}},
        "effective_policy": {"controlled": {"automatic_compaction": False, "compaction_threshold": None, "provider_retry": {"enabled": True, "max_retries": 0}, "request_timeout_seconds": None, "idle_timeout_seconds": None, "outer_attempt_timeout_seconds": 900, "model_reasoning": DEFAULT_THINKING, "output_token_ceiling": None, "provider_routing": {"require_parameters": True}, "sampling": {"temperature": None, "seed": None}}, "native": {"tool_execution": []}, "observability_unknown": []},
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

    def test_rejects_incorrect_all_token_total(self) -> None:
        value = result()
        value["usage"]["all_tokens"] = 4
        with self.assertRaises(ContractError):
            validate_result(value)

    def test_accepts_empty_final_text_for_a_terminal_model_failure(self) -> None:
        value = result()
        value["terminal"] = {"status": "failed", "code": "provider_error"}
        value["final_text"] = ""
        self.assertEqual(validate_result(value)["terminal"]["status"], "failed")

    def test_preserves_raw_prompt_total_when_cache_components_are_inconsistent(self) -> None:
        value = result()
        value["usage"].update({"input": 0, "prompt_total": 4, "cache_read": 8, "cache_write": 1, "output": 3, "generation": 3, "all_tokens": 7})
        self.assertEqual(validate_result(value)["usage"]["prompt_total"], 4)

    def test_rejects_input_larger_than_raw_prompt_total(self) -> None:
        value = result()
        value["usage"].update({"input": 5, "prompt_total": 4, "generation": 8})
        with self.assertRaises(ContractError):
            validate_result(value)

    def test_rejects_static_harness_decision(self) -> None:
        value = result(baseline="tea-static")
        value["harness"]["mode"] = "static"
        with self.assertRaises(ContractError):
            validate_result(value)

    def test_plan_randomization_is_seeded_and_has_every_condition(self) -> None:
        first, second = randomized_plan(3, 20260823), randomized_plan(3, 20260823)
        self.assertEqual(first, second)
        self.assertTrue(all(set(order) == {"pi-static", "tea-static", "tea-jit"} for order in first))

    def test_counterbalanced_static_schedule_alternates_ab_and_ba(self) -> None:
        schedule = randomized_plan(7, 41, ("pi-static", "tea-static"))
        self.assertLessEqual(abs(schedule.count(["pi-static", "tea-static"]) - schedule.count(["tea-static", "pi-static"])), 1)

    def test_tea_only_schedule_contains_only_tea_static(self) -> None:
        self.assertEqual(randomized_plan(3, 41, ("tea-static",)), [["tea-static"]] * 3)

    def test_three_condition_schedule_balances_positions_over_a_complete_block(self) -> None:
        schedule = randomized_plan(6, 41)
        for condition in ("pi-static", "tea-static", "tea-jit"):
            self.assertEqual([sum(order[position] == condition for order in schedule) for position in range(3)], [2, 2, 2])

    def test_toolchain_manifest_is_deterministic_for_one_path(self) -> None:
        environment = {"PATH": __import__("os").environ["PATH"]}
        first, second = toolchain_manifest(environment), toolchain_manifest(environment)
        self.assertEqual(first, second)
        self.assertEqual([entry["name"] for entry in first["executables"]], ["bash", "git", "curl", "node", "npm"])

    def test_repeat_lanes_start_concurrently_and_return_repeat_order(self) -> None:
        barrier = threading.Barrier(2)

        def one_lane(repeat: int, order: list[str]) -> tuple[int, list[str]]:
            barrier.wait(timeout=1)
            return repeat, order

        self.assertEqual(
            run_repeat_lanes([["pi-static", "tea-static"], ["tea-static", "pi-static"]], 2, one_lane),
            [(0, ["pi-static", "tea-static"]), (1, ["tea-static", "pi-static"])],
        )

    def test_parallel_repeat_default_and_bounds_are_explicit(self) -> None:
        arguments = ("express-3936-medium", "openrouter", DEFAULT_MODEL, DEFAULT_THINKING, None, 2, 1, __import__("pathlib").Path("/tmp/cache"), __import__("pathlib").Path("/tmp/work"), __import__("pathlib").Path("/tmp/out"))
        self.assertEqual(Config(*arguments).effective_parallel_repeats(), 2)
        self.assertEqual(Config(*arguments, parallel_repeats=1).effective_parallel_repeats(), 1)
        with self.assertRaises(ShootoutError):
            Config(*arguments, parallel_repeats=3).validate()

    def test_static_only_plan_excludes_jit(self) -> None:
        config = Config("express-3936-medium", "openrouter", DEFAULT_MODEL, DEFAULT_THINKING, None, 1, 20260823, __import__("pathlib").Path("/tmp/cache"), __import__("pathlib").Path("/tmp/work"), __import__("pathlib").Path("/tmp/out"), static_only=True)
        value = plan(config)
        self.assertEqual(value["conditions"], ["pi-static", "tea-static"])
        self.assertEqual(set(value["condition_order"][0]), {"pi-static", "tea-static"})
        self.assertEqual(len(value["validator_dependency_lockfile_sha256"]), 64)
        self.assertEqual(value["parallel_repeats"], 1)

    def test_tea_only_plan_is_a_single_static_baseline(self) -> None:
        config = Config("express-3936-medium", "openrouter", DEFAULT_MODEL, DEFAULT_THINKING, None, 1, 20260823, __import__("pathlib").Path("/tmp/cache"), __import__("pathlib").Path("/tmp/work"), __import__("pathlib").Path("/tmp/out"), tea_only=True)
        value = plan(config)
        self.assertTrue(config.static_only)
        self.assertEqual(value["conditions"], ["tea-static"])
        self.assertEqual(value["condition_order"], [["tea-static"]])
        self.assertTrue(value["tea_only"])

    def test_hard_case_accepts_unbounded_diagnostic_timeout(self) -> None:
        config = Config("express-4205-hard", "openrouter", DEFAULT_MODEL, DEFAULT_THINKING, None, 1, 20260823, __import__("pathlib").Path("/tmp/cache"), __import__("pathlib").Path("/tmp/work"), __import__("pathlib").Path("/tmp/out"), timeout_seconds=0, static_only=True)
        config.validate()
        value = plan(config)
        self.assertEqual(value["task"], "express-4205-hard")
        self.assertEqual(value["timeout_seconds"], 0)
        self.assertEqual(value["conditions"], ["pi-static", "tea-static"])

    def test_timeout_rejects_negative_values(self) -> None:
        config = Config("express-3936-medium", "openrouter", DEFAULT_MODEL, DEFAULT_THINKING, None, 1, 1, __import__("pathlib").Path("/tmp/cache"), __import__("pathlib").Path("/tmp/work"), __import__("pathlib").Path("/tmp/out"), timeout_seconds=-1)
        with self.assertRaises(ShootoutError):
            config.validate()

    def test_task_timeout_policy_gives_hard_case_more_headroom(self) -> None:
        self.assertEqual(TASK_TIMEOUT_SECONDS["express-3936-medium"], DEFAULT_TIMEOUT_SECONDS)
        self.assertEqual(TASK_TIMEOUT_SECONDS["express-4205-hard"], HARD_TIMEOUT_SECONDS)
        self.assertGreater(TASK_TIMEOUT_SECONDS["express-4205-hard"], TASK_TIMEOUT_SECONDS["express-3936-medium"])
        arguments = ("express-4205-hard", "openrouter", DEFAULT_MODEL, DEFAULT_THINKING, None, 1, 1, __import__("pathlib").Path("/tmp/cache"), __import__("pathlib").Path("/tmp/work"), __import__("pathlib").Path("/tmp/out"))
        self.assertEqual(Config(*arguments).timeout_seconds, HARD_TIMEOUT_SECONDS)

    def test_zero_timeout_waits_without_a_communicate_deadline(self) -> None:
        process = Mock()
        process.communicate.return_value = ("", "")
        process.returncode = 0
        with patch("evals.pi_shootout.runner.subprocess.Popen", return_value=process) as popen:
            result = _run_process(["adapter"], cwd=__import__("pathlib").Path("/tmp"), environment={}, timeout_seconds=0)
        popen.assert_called_once()
        process.communicate.assert_called_once_with()
        self.assertEqual(result[:2], (0, False))

    def test_configuration_requires_the_fixed_v0_model_and_unbounded_is_valid(self) -> None:
        config = Config("express-3936-medium", "openrouter", DEFAULT_MODEL, DEFAULT_THINKING, None, 1, 1, __import__("pathlib").Path("/tmp/cache"), __import__("pathlib").Path("/tmp/work"), __import__("pathlib").Path("/tmp/out"))
        config.validate()
        with self.assertRaises(ShootoutError):
            Config("express-3936-medium", "openrouter", "poolside/laguna-xs-2.1:free", DEFAULT_THINKING, None, 1, 1, config.cache_root, config.workspace_root, config.out).validate()
        with self.assertRaises(ShootoutError):
            Config("express-3936-medium", "openrouter", DEFAULT_MODEL, DEFAULT_THINKING, 4096, 1, 1, config.cache_root, config.workspace_root, config.out).validate()


if __name__ == "__main__":
    unittest.main()
