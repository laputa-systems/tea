from __future__ import annotations

import _thread
import contextlib
import copy
import hashlib
import io
import os
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import Mock, call, patch

from .__main__ import _config, parser
from .contract import (
    LEGACY_RESULT_SCHEMA,
    POST_EDIT_VALIDATION_BLOCK_REASON,
    POST_EDIT_VALIDATION_REMINDER,
    ContractError,
    RESULT_SCHEMA,
    validate_enriched_v3_result,
    validate_legacy_v3_result,
    validate_result,
)
from .runner import Config, DEFAULT_MODEL, DEFAULT_THINKING, DEFAULT_TIMEOUT_SECONDS, HARD_TIMEOUT_SECONDS, STOP_POLL_SECONDS, ShootoutError, TASK_TIMEOUT_SECONDS, _run_process, _signal_attempt_process_group, adapter_command, adapter_task, attempt_hard_timeout_seconds, capability_manifest, plan, randomized_plan, run_repeat_lanes, selected_case, toolchain_manifest


def pre_edit_tool_gate(mode: str = "none") -> dict:
    if mode == "none":
        return {
            "mode": "none",
            "blocked_tools": [],
            "target_restricted_tools": [],
            "source_local_targets": [],
            "unlocks_after": None,
            "same_batch_rule": None,
            "block_reason_sha256": None,
        }
    if mode == "direct-edit-v1":
        return {
            "mode": "direct-edit-v1",
            "blocked_tools": ["bash", "find"],
            "target_restricted_tools": [],
            "source_local_targets": [],
            "unlocks_after": "prior-successful-edit-result",
            "same_batch_rule": "block-until-prior-successful-edit-result",
            "block_reason_sha256": "a" * 64,
        }
    if mode == "source-local-v1":
        return {
            "mode": "source-local-v1",
            "blocked_tools": ["bash", "find"],
            "target_restricted_tools": ["read", "edit"],
            "source_local_targets": ["lib/response.js"],
            "unlocks_after": "prior-successful-target-local-edit-result",
            "same_batch_rule": "block-until-prior-successful-target-local-edit-result",
            "block_reason_sha256": "b" * 64,
        }
    raise ValueError(f"unsupported pre-edit gate test mode {mode!r}")


def post_edit_validation_gate(mode: str = "none") -> dict:
    if mode == "none":
        return {
            "mode": "none",
            "applies_after": None,
            "qualifies_with": None,
            "resets_after": None,
            "same_batch_rule": None,
            "command_profile": None,
            "completion_reminder_limit": 0,
            "block_reason_sha256": None,
            "reminder_sha256": None,
        }
    if mode == "unmasked-evidence-v1":
        return {
            "mode": "unmasked-evidence-v1",
            "applies_after": "prior-successful-declared-target-edit-result",
            "qualifies_with": "prior-successful-unmasked-direct-foreground-bash-result",
            "resets_after": "later-successful-edit-result",
            "same_batch_rule": "evidence-requires-prior-successful-bash-result",
            "command_profile": "unmasked-direct-foreground-bash/v1",
            "completion_reminder_limit": 1,
            "block_reason_sha256": hashlib.sha256(POST_EDIT_VALIDATION_BLOCK_REASON.encode()).hexdigest(),
            "reminder_sha256": hashlib.sha256(POST_EDIT_VALIDATION_REMINDER.encode()).hexdigest(),
        }
    raise ValueError(f"unsupported post-edit validation gate test mode {mode!r}")


def validation_evidence(state: str = "not_required") -> dict:
    value = {
        "state": state,
        "edit_generation": None,
        "qualifying_call_id_sha256": None,
        "qualifying_arguments_sha256": None,
        "qualifying_process_exit": None,
        "candidate_failures": 0,
        "masked_call_blocks": 0,
        "reminders_issued": 0,
        "transitions_sha256": "e" * 64,
    }
    if state == "satisfied":
        value.update({
            "edit_generation": 1,
            "qualifying_call_id_sha256": "c" * 64,
            "qualifying_arguments_sha256": "a" * 64,
            "qualifying_process_exit": "exited-zero",
        })
    elif state == "missing":
        value.update({"edit_generation": 1, "reminders_issued": 1})
    return value


def validation_transitions(state: str) -> list[dict]:
    if state == "not_required":
        return []
    entries = [
        {
            "type": "post_edit_validation_transition",
            "transition": "edit-pending",
            "generation": 1,
            "qualifying_call_id_sha256": None,
            "qualifying_arguments_sha256": None,
            "process_exit": None,
        },
    ]
    if state == "satisfied":
        entries.append({
            "type": "post_edit_validation_transition",
            "transition": "evidence-satisfied",
            "generation": 1,
            "qualifying_call_id_sha256": "c" * 64,
            "qualifying_arguments_sha256": "a" * 64,
            "process_exit": "exited-zero",
        })
    elif state == "missing":
        entries.extend([
            {
                "type": "post_edit_validation_transition",
                "transition": "completion-reminder-issued",
                "generation": 1,
                "qualifying_call_id_sha256": None,
                "qualifying_arguments_sha256": None,
                "process_exit": None,
            },
            {
                "type": "post_edit_validation_transition",
                "transition": "evidence-missing",
                "generation": 1,
                "qualifying_call_id_sha256": None,
                "qualifying_arguments_sha256": None,
                "process_exit": None,
            },
        ])
    return entries


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
        "surface": {"system_prompt_bytes": 1, "system_prompt_sha256": "a", "workspace_normalized_system_prompt_sha256": "b", "tool_surface_sha256": "c", "prompt_tool_surface_sha256": "prompt-tools", "wire_tool_surface_sha256": "wire-tools", "execution_surface_sha256": "execution", "active_tools": ["read", "bash", "edit", "find"], "authority": {"tools": ["read", "bash", "edit", "find"], "shell": True, "secret_boundary": "allowlist"}, "research_tools": [], "subagents": False, "shell_curl_available": True, "shell_environment_sha256": "d", "pre_edit_tool_gate": pre_edit_tool_gate(), "post_edit_validation_gate": post_edit_validation_gate()},
        "timings": {"agent_ms": 1, "candidate_validation_ms": 0, "rollover_ms": 0},
        "counts": {"turns": 1, "model_turns": 1, "provider_requests": None, "tool_calls": 0, "retries": 0, "compactions": 0},
        "usage": {"input": 2, "prompt_total": 2, "output": 3, "generation": 5, "all_tokens": 5, "reasoning": None, "cache_read": 0, "cache_write": 0},
        "cost": {"kind": "unavailable", "currency": "USD", "total": None},
        "wire": {"source": "direct-final-openrouter-boundary", "request_count": 1, "requests": [request], "routing_policy": {"require_parameters": True}, "returned_route": {"model": None, "provider": None, "provenance": None}},
        "effective_policy": {"controlled": {"automatic_compaction": False, "compaction_threshold": None, "provider_retry": {"enabled": True, "max_retries": 0}, "request_timeout_seconds": None, "idle_timeout_seconds": None, "outer_attempt_timeout_seconds": 900, "model_reasoning": DEFAULT_THINKING, "output_token_ceiling": None, "provider_routing": {"require_parameters": True}, "sampling": {"temperature": None, "seed": None}, "pre_edit_tool_gate": pre_edit_tool_gate(), "post_edit_validation_gate": post_edit_validation_gate()}, "native": {"tool_execution": []}, "observability_unknown": []},
        "harness": {"mode": "jit", "base_snapshot_id": "base", "initial_snapshot_id": "base", "final_snapshot_id": "base", "decision": "no-change", "candidate_count": 0, "candidate_id": None, "changed_surfaces": [], "candidate_source_bytes": 0, "hypothesis": None},
        "validation_evidence": validation_evidence(),
        "trace": [],
    }


class ContractTest(unittest.TestCase):
    def test_accepts_unbounded_output_and_generation_identity(self) -> None:
        self.assertEqual(validate_result(result())["usage"]["generation"], 5)

    def test_accepts_current_v4_result(self) -> None:
        self.assertEqual(result()["schema_version"], RESULT_SCHEMA)

    def test_accepts_complete_enriched_v3_result_only_through_compatibility_reader(self) -> None:
        value = result()
        value["schema_version"] = LEGACY_RESULT_SCHEMA
        self.assertEqual(validate_enriched_v3_result(value)["schema_version"], LEGACY_RESULT_SCHEMA)
        with self.assertRaises(ContractError):
            validate_result(value)

    def test_accepts_wholly_legacy_v3_result_only_through_compatibility_reader(self) -> None:
        value = result()
        value["schema_version"] = LEGACY_RESULT_SCHEMA
        value["surface"].pop("post_edit_validation_gate")
        value["effective_policy"]["controlled"].pop("post_edit_validation_gate")
        value.pop("validation_evidence")
        self.assertEqual(validate_legacy_v3_result(value)["schema_version"], LEGACY_RESULT_SCHEMA)
        with self.assertRaises(ContractError):
            validate_enriched_v3_result(value)

    def test_rejects_partial_v3_post_edit_evidence_shapes(self) -> None:
        for field in ("surface", "effective_policy.controlled", "validation_evidence"):
            with self.subTest(field=field):
                value = result()
                value["schema_version"] = LEGACY_RESULT_SCHEMA
                if field == "surface":
                    value["surface"].pop("post_edit_validation_gate")
                elif field == "effective_policy.controlled":
                    value["effective_policy"]["controlled"].pop("post_edit_validation_gate")
                else:
                    value.pop("validation_evidence")
                with self.assertRaises(ContractError):
                    validate_legacy_v3_result(value)
                with self.assertRaises(ContractError):
                    validate_enriched_v3_result(value)

    def test_rejects_a_legacy_v3_trace_with_residual_post_edit_evidence(self) -> None:
        value = result()
        value["schema_version"] = LEGACY_RESULT_SCHEMA
        value["surface"].pop("post_edit_validation_gate")
        value["effective_policy"]["controlled"].pop("post_edit_validation_gate")
        value.pop("validation_evidence")
        value["trace"] = validation_transitions("missing")
        with self.assertRaises(ContractError):
            validate_legacy_v3_result(value)

    def test_requires_identical_pre_edit_gate_evidence_in_surface_and_policy(self) -> None:
        value = result()
        value["surface"]["pre_edit_tool_gate"] = pre_edit_tool_gate("direct-edit-v1")
        with self.assertRaises(ContractError):
            validate_result(value)

    def test_accepts_content_free_post_edit_validation_evidence(self) -> None:
        value = result(baseline="tea-static")
        source_local = pre_edit_tool_gate("source-local-v1")
        gate = post_edit_validation_gate("unmasked-evidence-v1")
        value["surface"]["pre_edit_tool_gate"] = source_local
        value["effective_policy"]["controlled"]["pre_edit_tool_gate"] = source_local
        value["surface"]["post_edit_validation_gate"] = gate
        value["effective_policy"]["controlled"]["post_edit_validation_gate"] = gate
        value["validation_evidence"] = validation_evidence("satisfied")
        value["trace"] = validation_transitions("satisfied")
        value["harness"] = {
            "mode": "static",
            "base_snapshot_id": "base",
            "initial_snapshot_id": "base",
            "final_snapshot_id": "base",
            "decision": "not-applicable",
            "candidate_count": 0,
            "candidate_id": None,
            "changed_surfaces": [],
            "candidate_source_bytes": 0,
            "hypothesis": None,
        }
        self.assertEqual(validate_result(value)["validation_evidence"]["state"], "satisfied")

    def test_rejects_post_edit_validation_evidence_that_exposes_command_content(self) -> None:
        value = result(baseline="pi-static")
        source_local = pre_edit_tool_gate("source-local-v1")
        gate = post_edit_validation_gate("unmasked-evidence-v1")
        value["surface"]["pre_edit_tool_gate"] = source_local
        value["effective_policy"]["controlled"]["pre_edit_tool_gate"] = source_local
        value["surface"]["post_edit_validation_gate"] = gate
        value["effective_policy"]["controlled"]["post_edit_validation_gate"] = gate
        value["validation_evidence"] = validation_evidence("missing")
        value["trace"] = validation_transitions("missing")
        value["trace"][1]["command"] = "npm test"
        value["harness"] = {
            "mode": "static",
            "base_snapshot_id": "base",
            "initial_snapshot_id": "base",
            "final_snapshot_id": "base",
            "decision": "not-applicable",
            "candidate_count": 0,
            "candidate_id": None,
            "changed_surfaces": [],
            "candidate_source_bytes": 0,
            "hypothesis": None,
        }
        with self.assertRaises(ContractError):
            validate_result(value)

    def test_post_edit_validation_gate_requires_source_local_static_result(self) -> None:
        value = result(baseline="tea-static")
        gate = post_edit_validation_gate("unmasked-evidence-v1")
        value["surface"]["post_edit_validation_gate"] = gate
        value["effective_policy"]["controlled"]["post_edit_validation_gate"] = gate
        with self.assertRaises(ContractError):
            validate_result(value)

    def test_post_edit_validation_satisfied_evidence_requires_an_exit_zero_process_witness(self) -> None:
        value = result(baseline="tea-static")
        source_local = pre_edit_tool_gate("source-local-v1")
        gate = post_edit_validation_gate("unmasked-evidence-v1")
        value["surface"]["pre_edit_tool_gate"] = source_local
        value["effective_policy"]["controlled"]["pre_edit_tool_gate"] = source_local
        value["surface"]["post_edit_validation_gate"] = gate
        value["effective_policy"]["controlled"]["post_edit_validation_gate"] = gate
        value["validation_evidence"] = validation_evidence("satisfied")
        value["trace"] = validation_transitions("satisfied")
        value["trace"][1]["process_exit"] = None
        value["harness"].update({"mode": "static", "decision": "not-applicable"})
        with self.assertRaises(ContractError):
            validate_result(value)

    def test_later_successful_native_edit_resets_prior_post_edit_validation_evidence(self) -> None:
        value = result(baseline="pi-static")
        source_local = pre_edit_tool_gate("source-local-v1")
        gate = post_edit_validation_gate("unmasked-evidence-v1")
        value["surface"]["pre_edit_tool_gate"] = source_local
        value["effective_policy"]["controlled"]["pre_edit_tool_gate"] = source_local
        value["surface"]["post_edit_validation_gate"] = gate
        value["effective_policy"]["controlled"]["post_edit_validation_gate"] = gate
        value["validation_evidence"] = validation_evidence("missing")
        value["validation_evidence"]["edit_generation"] = 2
        value["trace"] = validation_transitions("satisfied") + [
            {
                "type": "post_edit_validation_transition",
                "transition": "edit-pending",
                "generation": 2,
                "qualifying_call_id_sha256": None,
                "qualifying_arguments_sha256": None,
                "process_exit": None,
            },
            {
                "type": "post_edit_validation_transition",
                "transition": "completion-reminder-issued",
                "generation": 2,
                "qualifying_call_id_sha256": None,
                "qualifying_arguments_sha256": None,
                "process_exit": None,
            },
            {
                "type": "post_edit_validation_transition",
                "transition": "evidence-missing",
                "generation": 2,
                "qualifying_call_id_sha256": None,
                "qualifying_arguments_sha256": None,
                "process_exit": None,
            },
        ]
        value["harness"].update({"mode": "static", "decision": "not-applicable"})
        self.assertEqual(validate_result(value)["validation_evidence"]["edit_generation"], 2)

    def test_accepts_the_target_restricted_source_local_gate(self) -> None:
        value = result()
        source_local = pre_edit_tool_gate("source-local-v1")
        value["surface"]["pre_edit_tool_gate"] = source_local
        value["effective_policy"]["controlled"]["pre_edit_tool_gate"] = source_local
        self.assertEqual(
            validate_result(value)["surface"]["pre_edit_tool_gate"]["source_local_targets"],
            ["lib/response.js"],
        )

    def test_rejects_an_unsafe_source_local_target(self) -> None:
        value = result()
        source_local = pre_edit_tool_gate("source-local-v1")
        source_local["source_local_targets"] = ["../outside.js"]
        value["surface"]["pre_edit_tool_gate"] = source_local
        value["effective_policy"]["controlled"]["pre_edit_tool_gate"] = source_local
        with self.assertRaises(ContractError):
            validate_result(value)

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

        def one_lane(repeat: int, order: list[str], _cancellation: threading.Event | None = None) -> tuple[int, list[str]]:
            barrier.wait(timeout=1)
            return repeat, order

        self.assertEqual(
            run_repeat_lanes([["pi-static", "tea-static"], ["tea-static", "pi-static"]], 2, one_lane),
            [(0, ["pi-static", "tea-static"]), (1, ["tea-static", "pi-static"])],
        )

    def test_parallel_interrupt_notifies_cooperative_lanes_before_reraising(self) -> None:
        lanes_started = threading.Barrier(3)
        cancellation_observed = threading.Event()

        def one_lane(repeat: int, order: list[str], cancellation: threading.Event | None = None) -> tuple[int, list[str]]:
            lanes_started.wait(timeout=1)
            if cancellation is None:
                time.sleep(1.5)
                return repeat, order
            self.assertTrue(cancellation.wait(timeout=1), "parallel lane did not receive controller cancellation")
            cancellation_observed.set()
            return repeat, order

        interrupter = threading.Thread(target=lambda: (lanes_started.wait(timeout=1), _thread.interrupt_main()))
        interrupter.start()
        started = time.monotonic()
        try:
            with self.assertRaises(KeyboardInterrupt):
                run_repeat_lanes([["pi-static"], ["tea-static"]], 2, one_lane)
        finally:
            interrupter.join(timeout=2)
        self.assertLess(time.monotonic() - started, 0.8, "controller waited for uncancelled parallel lanes")
        self.assertTrue(cancellation_observed.is_set(), "parallel lanes did not observe controller cancellation")

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

    def test_tool_child_sandbox_is_explicit_tea_only_diagnostic_policy(self) -> None:
        root = __import__("pathlib").Path("/tmp")
        config = Config(
            "express-3936-medium",
            "openrouter",
            DEFAULT_MODEL,
            DEFAULT_THINKING,
            None,
            1,
            20260823,
            root / "cache",
            root / "work",
            root / "out",
            tea_only=True,
            tool_child_sandbox="macos-seatbelt-v1",
        )
        config.validate()
        self.assertEqual(plan(config)["tool_child_sandbox"], "macos-seatbelt-v1")
        command = adapter_command(
            config,
            "tea-static",
            task=root / "task.json",
            workspace=root / "workspace",
            capabilities=root / "capabilities.json",
            result=root / "result.json",
            evidence=root / "evidence",
            attempt_id="shootout-r1-tea-static",
            shell_environment={},
        )
        self.assertIn("--tool-child-sandbox", command)
        self.assertIn("macos-seatbelt-v1", command)
        with self.assertRaises(ShootoutError):
            Config(
                "express-3936-medium",
                "openrouter",
                DEFAULT_MODEL,
                DEFAULT_THINKING,
                None,
                1,
                20260823,
                root / "cache",
                root / "work",
                root / "out",
                static_only=True,
                tool_child_sandbox="macos-seatbelt-v1",
            ).validate()

    def test_git_hiding_tool_child_sandbox_is_recorded_as_tea_only_diagnostic_policy(self) -> None:
        root = __import__("pathlib").Path("/tmp")
        config = Config(
            "express-3936-medium",
            "openrouter",
            DEFAULT_MODEL,
            DEFAULT_THINKING,
            None,
            1,
            20260823,
            root / "cache",
            root / "work",
            root / "out",
            tea_only=True,
            tool_child_sandbox="macos-seatbelt-v2",
        )
        config.validate()
        self.assertEqual(plan(config)["tool_child_sandbox"], "macos-seatbelt-v2")
        command = adapter_command(
            config,
            "tea-static",
            task=root / "task.json",
            workspace=root / "workspace",
            capabilities=root / "capabilities.json",
            result=root / "result.json",
            evidence=root / "evidence",
            attempt_id="shootout-r1-tea-static",
            shell_environment={},
        )
        self.assertIn("--tool-child-sandbox", command)
        self.assertIn("macos-seatbelt-v2", command)

    def test_edit_recovery_projection_is_explicit_tea_only_diagnostic_policy(self) -> None:
        root = __import__("pathlib").Path("/tmp")
        config = Config(
            "express-3936-medium",
            "openrouter",
            DEFAULT_MODEL,
            DEFAULT_THINKING,
            None,
            1,
            20260823,
            root / "cache",
            root / "work",
            root / "out",
            tea_only=True,
            edit_recovery_projection="canonical-v1",
        )
        config.validate()
        self.assertEqual(plan(config)["edit_recovery_projection"], "canonical-v1")
        command = adapter_command(
            config,
            "tea-static",
            task=root / "task.json",
            workspace=root / "workspace",
            capabilities=root / "capabilities.json",
            result=root / "result.json",
            evidence=root / "evidence",
            attempt_id="shootout-r1-tea-static",
            shell_environment={},
        )
        self.assertIn("--edit-recovery-projection", command)
        self.assertIn("canonical-v1", command)
        with self.assertRaises(ShootoutError):
            Config(
                "express-3936-medium",
                "openrouter",
                DEFAULT_MODEL,
                DEFAULT_THINKING,
                None,
                1,
                20260823,
                root / "cache",
                root / "work",
                root / "out",
                static_only=True,
                edit_recovery_projection="canonical-v1",
            ).validate()

    def test_no_history_static_prompt_profile_is_explicit_and_forwarded(self) -> None:
        root = __import__("pathlib").Path("/tmp")
        config = Config(
            "express-3936-medium",
            "openrouter",
            DEFAULT_MODEL,
            DEFAULT_THINKING,
            None,
            1,
            20260823,
            root / "cache",
            root / "work",
            root / "out",
            tea_only=True,
            static_prompt_profile="no-history-v1",
        )
        config.validate()
        self.assertEqual(plan(config)["static_prompt_profile"], "no-history-v1")
        command = adapter_command(
            config,
            "tea-static",
            task=root / "task.json",
            workspace=root / "workspace",
            capabilities=root / "capabilities.json",
            result=root / "result.json",
            evidence=root / "evidence",
            attempt_id="shootout-r1-tea-static",
            shell_environment={},
        )
        self.assertIn("--static-prompt-profile", command)
        self.assertIn("no-history-v1", command)
        with self.assertRaises(ShootoutError):
            Config(
                "express-3936-medium",
                "openrouter",
                DEFAULT_MODEL,
                DEFAULT_THINKING,
                None,
                1,
                20260823,
                root / "cache",
                root / "work",
                root / "out",
                static_prompt_profile="no-history-v1",
            ).validate()

    def test_no_history_static_prompt_profile_rejects_a_paired_static_config(self) -> None:
        root = Path("/tmp")
        with self.assertRaisesRegex(ShootoutError, "Tea-only diagnostic"):
            Config(
                "express-3936-medium",
                "openrouter",
                DEFAULT_MODEL,
                DEFAULT_THINKING,
                None,
                1,
                20260823,
                root / "cache",
                root / "work",
                root / "out",
                static_only=True,
                static_prompt_profile="no-history-v1",
            ).validate()

    def test_prefix_guard_static_prompt_profile_is_tea_only_and_forwarded(self) -> None:
        root = __import__("pathlib").Path("/tmp")
        config = Config(
            "express-4205-hard",
            "openrouter",
            DEFAULT_MODEL,
            DEFAULT_THINKING,
            None,
            1,
            20260823,
            root / "cache",
            root / "work",
            root / "out",
            tea_only=True,
            static_prompt_profile="prefix-guard-v1",
        )
        config.validate()
        self.assertEqual(plan(config)["static_prompt_profile"], "prefix-guard-v1")
        command = adapter_command(
            config,
            "tea-static",
            task=root / "task.json",
            workspace=root / "workspace",
            capabilities=root / "capabilities.json",
            result=root / "result.json",
            evidence=root / "evidence",
            attempt_id="shootout-r1-tea-static",
            shell_environment={},
        )
        self.assertIn("--static-prompt-profile", command)
        self.assertIn("prefix-guard-v1", command)
        with self.assertRaises(ShootoutError):
            Config(
                "express-4205-hard",
                "openrouter",
                DEFAULT_MODEL,
                DEFAULT_THINKING,
                None,
                1,
                20260823,
                root / "cache",
                root / "work",
                root / "out",
                static_only=True,
                static_prompt_profile="prefix-guard-v1",
            ).validate()

    def test_focused_prefix_guard_static_prompt_profile_is_tea_only(self) -> None:
        root = __import__("pathlib").Path("/tmp")
        config = Config(
            "express-4205-hard",
            "openrouter",
            DEFAULT_MODEL,
            DEFAULT_THINKING,
            None,
            1,
            20260823,
            root / "cache",
            root / "work",
            root / "out",
            tea_only=True,
            static_prompt_profile="prefix-guard-focused-v1",
        )
        config.validate()
        self.assertEqual(plan(config)["static_prompt_profile"], "prefix-guard-focused-v1")
        command = adapter_command(
            config,
            "tea-static",
            task=root / "task.json",
            workspace=root / "workspace",
            capabilities=root / "capabilities.json",
            result=root / "result.json",
            evidence=root / "evidence",
            attempt_id="shootout-r1-tea-static",
            shell_environment={},
        )
        self.assertIn("--static-prompt-profile", command)
        self.assertIn("prefix-guard-focused-v1", command)
        with self.assertRaises(ShootoutError):
            Config(
                "express-4205-hard",
                "openrouter",
                DEFAULT_MODEL,
                DEFAULT_THINKING,
                None,
                1,
                20260823,
                root / "cache",
                root / "work",
                root / "out",
                static_only=True,
                static_prompt_profile="prefix-guard-focused-v1",
            ).validate()

    def test_direct_edit_gate_is_a_static_paired_policy_forwarded_to_both_adapters(self) -> None:
        root = __import__("pathlib").Path("/tmp")
        config = Config(
            "express-4205-hard",
            "openrouter",
            DEFAULT_MODEL,
            DEFAULT_THINKING,
            None,
            1,
            20260823,
            root / "cache",
            root / "work",
            root / "out",
            static_only=True,
            pre_edit_tool_gate="direct-edit-v1",
        )
        config.validate()
        self.assertEqual(plan(config)["pre_edit_tool_gate"], "direct-edit-v1")
        self.assertEqual(plan(config)["conditions"], ["pi-static", "tea-static"])
        for baseline in ("pi-static", "tea-static"):
            command = adapter_command(
                config,
                baseline,
                task=root / "task.json",
                workspace=root / "workspace",
                capabilities=root / "capabilities.json",
                result=root / "result.json",
                evidence=root / "evidence",
                attempt_id=f"shootout-r1-{baseline}",
                shell_environment={},
            )
            self.assertIn("--pre-edit-tool-gate", command)
            self.assertIn("direct-edit-v1", command)
        with self.assertRaises(ShootoutError):
            Config(
                "express-4205-hard",
                "openrouter",
                DEFAULT_MODEL,
                DEFAULT_THINKING,
                None,
                1,
                20260823,
                root / "cache",
                root / "work",
                root / "out",
                pre_edit_tool_gate="direct-edit-v1",
            ).validate()

    def test_source_local_gate_is_fresh_static_paired_and_copies_versioned_targets(self) -> None:
        root = __import__("pathlib").Path("/tmp")
        config = Config(
            "express-3936-medium",
            "openrouter",
            DEFAULT_MODEL,
            DEFAULT_THINKING,
            None,
            1,
            20260823,
            root / "cache",
            root / "work",
            root / "out",
            static_only=True,
            pre_edit_tool_gate="source-local-v1",
        )
        config.validate()
        self.assertEqual(plan(config)["source_local_targets"], ["lib/response.js"])
        task = adapter_task(selected_case("express-3936-medium"), capability_manifest(), 900)
        self.assertEqual(task["source_local_v1"], {
            "schema_version": "tea-coding-eval-source-local/v1",
            "targets": ["lib/response.js"],
        })
        for baseline in ("pi-static", "tea-static"):
            command = adapter_command(
                config,
                baseline,
                task=root / "task.json",
                workspace=root / "workspace",
                capabilities=root / "capabilities.json",
                result=root / "result.json",
                evidence=root / "evidence",
                attempt_id=f"shootout-r1-{baseline}",
                shell_environment={},
            )
            self.assertIn("source-local-v1", command)
        with self.assertRaises(ShootoutError):
            Config(
                "express-3936-medium",
                "openrouter",
                DEFAULT_MODEL,
                DEFAULT_THINKING,
                None,
                1,
                20260823,
                root / "cache",
                root / "work",
                root / "out",
                tea_only=True,
                pre_edit_tool_gate="source-local-v1",
            ).validate()

    def test_post_edit_validation_gate_is_paired_source_local_cli_and_config_policy(self) -> None:
        root = Path("/tmp")
        arguments = parser().parse_args([
            "plan",
            "--static-only",
            "--pre-edit-tool-gate",
            "source-local-v1",
            "--post-edit-validation-gate",
            "unmasked-evidence-v1",
        ])
        config = _config(arguments)
        config.validate()
        self.assertEqual(config.post_edit_validation_gate, "unmasked-evidence-v1")
        self.assertEqual(plan(config)["post_edit_validation_gate"], "unmasked-evidence-v1")
        for baseline in ("pi-static", "tea-static"):
            command = adapter_command(
                config,
                baseline,
                task=root / "task.json",
                workspace=root / "workspace",
                capabilities=root / "capabilities.json",
                result=root / "result.json",
                evidence=root / "evidence",
                attempt_id=f"shootout-r1-{baseline}",
                shell_environment={},
            )
            self.assertIn("--post-edit-validation-gate", command)
            self.assertIn("unmasked-evidence-v1", command)

    def test_post_edit_validation_gate_rejects_non_source_local_tea_only_and_jit_configs(self) -> None:
        root = Path("/tmp")
        common = (
            "express-3936-medium",
            "openrouter",
            DEFAULT_MODEL,
            DEFAULT_THINKING,
            None,
            1,
            20260823,
            root / "cache",
            root / "work",
            root / "out",
        )
        for options in (
            {"static_only": True, "post_edit_validation_gate": "unmasked-evidence-v1"},
            {
                "tea_only": True,
                "pre_edit_tool_gate": "source-local-v1",
                "post_edit_validation_gate": "unmasked-evidence-v1",
            },
            {
                "pre_edit_tool_gate": "source-local-v1",
                "post_edit_validation_gate": "unmasked-evidence-v1",
            },
        ):
            with self.subTest(options=options), self.assertRaises(ShootoutError):
                Config(*common, **options).validate()
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            parser().parse_args([
                "plan",
                "--post-edit-validation-gate",
                "unsupported-v1",
            ])

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

    def test_zero_timeout_polls_without_an_outer_deadline(self) -> None:
        process = Mock()
        process.communicate.return_value = ("", "")
        process.returncode = 0
        with patch("evals.pi_shootout.runner.subprocess.Popen", return_value=process) as popen, patch("evals.pi_shootout.runner._attempt_session_groups", return_value=()):
            result = _run_process(["adapter"], cwd=__import__("pathlib").Path("/tmp"), environment={}, timeout_seconds=0)
        popen.assert_called_once()
        process.communicate.assert_called_once_with(timeout=STOP_POLL_SECONDS)
        self.assertEqual((result.exit_code, result.timed_out), (0, False))
        self.assertEqual(result.finalization.status, "settled")

    @unittest.skipUnless(__import__("os").name == "posix", "process-group cleanup is POSIX-specific")
    def test_interrupt_terminates_the_isolated_adapter_process_group(self) -> None:
        process = Mock()
        process.pid = 123
        process.communicate.side_effect = KeyboardInterrupt
        with patch("evals.pi_shootout.runner.subprocess.Popen", return_value=process), patch("evals.pi_shootout.runner._attempt_process_groups", return_value=[123]), patch("evals.pi_shootout.runner.os.killpg") as killpg:
            with self.assertRaises(KeyboardInterrupt):
                _run_process(["adapter"], cwd=__import__("pathlib").Path("/tmp"), environment={}, timeout_seconds=1)
        self.assertEqual(killpg.call_args_list, [call(123, signal.SIGTERM), call(123, signal.SIGKILL)])

    @unittest.skipUnless(os.name == "posix", "process-group cleanup is POSIX-specific")
    def test_attempt_cleanup_stops_a_nested_tool_process_group(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            escaped_marker = Path(directory) / "nested-tool-escaped"
            ready_marker = Path(directory) / "nested-tool-ready"
            child_program = (
                "import pathlib, signal, time\n"
                "signal.signal(signal.SIGTERM, lambda *_: None)\n"
                f"pathlib.Path({str(ready_marker)!r}).write_text('ready')\n"
                "time.sleep(0.5)\n"
                f"pathlib.Path({str(escaped_marker)!r}).write_text('escaped')\n"
                "time.sleep(60)\n"
            )
            root_program = (
                "import pathlib, subprocess, sys, time\n"
                f"child = subprocess.Popen([sys.executable, '-c', {child_program!r}], start_new_session=True)\n"
                f"while not pathlib.Path({str(ready_marker)!r}).exists(): time.sleep(0.01)\n"
                "print(child.pid, flush=True)\n"
                "time.sleep(60)\n"
            )
            root = subprocess.Popen(
                [sys.executable, "-c", root_program],
                text=True,
                stdout=subprocess.PIPE,
                start_new_session=True,
            )
            child_pid = int(root.stdout.readline())
            try:
                _signal_attempt_process_group(root, signal.SIGTERM)
                root.wait(timeout=5)
                time.sleep(1)
                self.assertFalse(
                    escaped_marker.exists(),
                    "nested tool group survived attempt cleanup",
                )
            finally:
                if root.poll() is None:
                    os.killpg(root.pid, signal.SIGKILL)
                try:
                    os.kill(child_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                if root.stdout is not None:
                    root.stdout.close()

    def test_static_adapters_reserve_finalization_grace(self) -> None:
        self.assertEqual(attempt_hard_timeout_seconds("tea-static", 1_800), 1_815)
        self.assertEqual(attempt_hard_timeout_seconds("tea-static", 900), 915)
        self.assertEqual(attempt_hard_timeout_seconds("pi-static", 1_800), 1_815)
        self.assertEqual(attempt_hard_timeout_seconds("pi-static", 900), 915)
        self.assertEqual(attempt_hard_timeout_seconds("tea-jit", 1_800), 1_800)
        self.assertEqual(attempt_hard_timeout_seconds("tea-static", 0), 0)

    def test_configuration_requires_the_fixed_v0_model_and_unbounded_is_valid(self) -> None:
        config = Config("express-3936-medium", "openrouter", DEFAULT_MODEL, DEFAULT_THINKING, None, 1, 1, __import__("pathlib").Path("/tmp/cache"), __import__("pathlib").Path("/tmp/work"), __import__("pathlib").Path("/tmp/out"))
        config.validate()
        with self.assertRaises(ShootoutError):
            Config("express-3936-medium", "openrouter", "poolside/laguna-xs-2.1:free", DEFAULT_THINKING, None, 1, 1, config.cache_root, config.workspace_root, config.out).validate()
        with self.assertRaises(ShootoutError):
            Config("express-3936-medium", "openrouter", DEFAULT_MODEL, DEFAULT_THINKING, 4096, 1, 1, config.cache_root, config.workspace_root, config.out).validate()


if __name__ == "__main__":
    unittest.main()
