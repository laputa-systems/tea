from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from .compare import ComparisonError, compare_run, render_markdown, write_comparison
from .contract import LEGACY_RESULT_SCHEMA, RESULT_SCHEMA
from .runner import DEFAULT_MODEL, DEFAULT_THINKING
from .test_contract import (
    post_edit_validation_gate,
    pre_edit_tool_gate,
    result as valid_result,
    validation_evidence,
    validation_transitions,
)


def _run_metadata() -> dict:
    return {
        "run_id": "run",
        "task_id": "express-3936-medium",
        "task_manifest_sha256": "task",
        "baseline_commit": "base",
        "validator_sha256": "validator",
        "model": DEFAULT_MODEL,
        "provider": "openrouter",
        "thinking_level": DEFAULT_THINKING,
        "max_output_tokens": None,
        "timeout_seconds": 900,
        "provider_routing": {"require_parameters": True},
        "toolchain_manifest_sha256": "toolchain",
        "run_class": "smoke-diagnostic",
        "result_schema": RESULT_SCHEMA,
        "pre_edit_tool_gate": "none",
        "post_edit_validation_gate": "none",
        "source_local_targets": [],
        "condition_order": ["pi-static", "tea-static"],
    }


def _record(baseline: str, attempt_id: str, *, model_turns: int, tool_calls: int, generation: int) -> dict:
    value = valid_result(baseline=baseline)
    value["attempt_id"] = attempt_id
    value["runtime"]["implementation"] = "pi-sdk" if baseline == "pi-static" else "tea"
    value["counts"]["model_turns"] = model_turns
    value["counts"]["tool_calls"] = tool_calls
    value["usage"]["input"] = generation - value["usage"]["output"]
    value["usage"]["generation"] = generation
    value["usage"]["prompt_total"] = value["usage"]["input"]
    value["usage"]["all_tokens"] = generation
    if baseline == "pi-static":
        value["trace"] = [
            {"type": "turn_start"},
            {"type": "tool_execution_start", "tool_name": "bash", "tool_call_id": "pi-call", "arguments_sha256": "arguments", "category": "validation"},
        ]
    return {
        "baseline_id": baseline,
        "attempt_id": attempt_id,
        "adapter_result": value,
        "validator": {"passed": True},
        "process": {"peak_rss_bytes": None},
        "timings": {"total_attempt_ms": 1},
        "patch_sha256": "patch",
        "initial_workspace_state": {"commit": "base", "tree_sha256": "tree"},
    }


def _tea_session(path: Path, *, requests: int = 1) -> None:
    rows: list[dict] = []
    seq = 1
    for ordinal in range(requests):
        rows.append(
            {
                "seq": seq,
                "mutation": {
                    "kind": "record",
                    "payload": {
                        "type": "provider_request_settled",
                        "outcome": {"status": "settled", "stop_reason": "tool_use" if ordinal + 1 < requests else "stop"},
                        "usage": {"input_tokens": 4 + ordinal, "output_tokens": 2, "reasoning_tokens": 0, "cache_read_tokens": ordinal, "cache_write_tokens": 0},
                    },
                },
            }
        )
        seq += 1
        call_id = f"call-{ordinal}"
        rows.append(
            {
                "seq": seq,
                "mutation": {
                    "kind": "entry",
                    "payload": {
                        "entry": {
                            "type": "assistant_message",
                            "content": "",
                            "stop_reason": "tool_use" if ordinal + 1 < requests else "stop",
                            "tool_calls": [{"id": call_id, "name": "bash", "arguments": {"command": "npm install" if ordinal == 0 else "npm test"}}] if ordinal + 1 < requests else [],
                        }
                    },
                },
            }
        )
        seq += 1
        if ordinal + 1 < requests:
            rows.append(
                {
                    "seq": seq,
                    "mutation": {
                        "kind": "entry",
                        "payload": {
                            "entry": {"type": "tool_result", "tool_call_id": call_id, "is_error": False}
                        },
                    },
                }
            )
            seq += 1
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(json.dumps(row) for row in rows) + "\n", encoding="utf-8")


def _summary(directory: Path, *, repeats: int = 1) -> None:
    attempts = []
    for repeat in range(1, repeats + 1):
        pi_id = f"shootout-r{repeat}-pi-static"
        tea_id = f"shootout-r{repeat}-tea-static"
        pi = _record("pi-static", pi_id, model_turns=1, tool_calls=1, generation=10 + repeat)
        tea = _record("tea-static", tea_id, model_turns=2, tool_calls=2, generation=12 + repeat)
        attempts.extend([pi, tea])
        for record in (pi, tea):
            name = f"r{repeat}-{record['baseline_id']}"
            (directory / "attempts" / name).mkdir(parents=True, exist_ok=True)
            (directory / "attempts" / name / "record.json").write_text(json.dumps(record), encoding="utf-8")
        _tea_session(directory / "attempts" / f"r{repeat}-tea-static" / "harness" / "session.tea" / "session.jsonl", requests=2)
    (directory / "summary.json").write_text(json.dumps({"schema_version": "tea-pi-shootout-summary/v1", "run": _run_metadata(), "attempts": attempts}), encoding="utf-8")


def _enriched_v3(summary: dict) -> None:
    summary["run"]["result_schema"] = LEGACY_RESULT_SCHEMA
    for attempt in summary["attempts"]:
        attempt["adapter_result"]["schema_version"] = LEGACY_RESULT_SCHEMA


def _legacy_v3(summary: dict) -> None:
    _enriched_v3(summary)
    summary["run"].pop("post_edit_validation_gate")
    for attempt in summary["attempts"]:
        result = attempt["adapter_result"]
        result["surface"].pop("post_edit_validation_gate")
        result["effective_policy"]["controlled"].pop("post_edit_validation_gate")
        result.pop("validation_evidence")


def _fixed_static_conditions(summary: dict, *, normalize_tea_generation: bool) -> None:
    for attempt in summary["attempts"]:
        result = attempt["adapter_result"]
        result["model"]["sampling"] = {"temperature": 0, "seed": 20260829, "source": "adapter-set"}
        result["effective_policy"]["controlled"]["sampling"] = {"temperature": 0, "seed": 20260829}
        result["effective_policy"]["controlled"]["request_timeout_seconds"] = 1800
        result["effective_policy"]["controlled"]["idle_timeout_seconds"] = 1800
        result["wire"]["returned_route"] = {"model": "deepseek-v4-flash", "provider": "deepseek", "provenance": "header"}
        request = result["wire"]["requests"][0]
        request["temperature"] = {"present": True, "value": 0}
        request["seed"] = {"present": True, "value": 20260829}
        if normalize_tea_generation and attempt["baseline_id"] == "tea-static":
            result["usage"]["input"] -= 2
            result["usage"]["prompt_total"] -= 2
            result["usage"]["generation"] -= 2
            result["usage"]["all_tokens"] -= 2


class CompareTest(unittest.TestCase):
    def test_fixed_sampling_is_not_reported_as_a_variance_unknown(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory)
            summary = json.loads((directory / "summary.json").read_text())
            for attempt in summary["attempts"]:
                result = attempt["adapter_result"]
                result["model"]["sampling"] = {"temperature": 0, "seed": 20260829, "source": "adapter-set"}
                result["effective_policy"]["controlled"]["sampling"] = {"temperature": 0, "seed": 20260829}
                request = result["wire"]["requests"][0]
                request["temperature"] = {"present": True, "value": 0}
                request["seed"] = {"present": True, "value": 20260829}
            (directory / "summary.json").write_text(json.dumps(summary))
            analysis = compare_run(directory)
        self.assertFalse(any("Sampling is not fixed" in unknown for unknown in analysis["unknowns"]))

    def test_reports_normalized_delta_and_durable_turn_categories(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory)
            analysis = compare_run(directory)
        self.assertTrue(analysis["comparable"])
        pair = analysis["pairs"][0]
        self.assertEqual(pair["delta_tea_minus_pi"]["usage"]["generation"], 2)
        self.assertEqual(pair["delta_tea_minus_pi"]["counts"]["model_turns"], 1)
        tea_turns = pair["tea"]["trace"]["turn_evidence"]
        self.assertEqual(tea_turns["source"], "tea-durable-session")
        self.assertEqual(tea_turns["turns"][0]["categories"], {"upstream_or_dependency": 1})
        self.assertEqual(pair["pi"]["trace"]["turn_evidence"]["turns"][0]["categories"], {"validation": 1})
        self.assertEqual(analysis["run"]["pre_edit_tool_gate"], "none")

    def test_enriched_v3_artifacts_remain_strictly_comparable_during_v4_transition(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory, repeats=3)
            summary = json.loads((directory / "summary.json").read_text())
            _fixed_static_conditions(summary, normalize_tea_generation=True)
            _enriched_v3(summary)
            (directory / "summary.json").write_text(json.dumps(summary))
            analysis = compare_run(directory)
        self.assertEqual(
            analysis["artifact_compatibility"]["profile"],
            "enriched-v3-post-edit-validation/v1",
        )
        self.assertTrue(analysis["strict_efficiency_conclusion"])

    def test_wholly_legacy_v3_artifacts_remain_comparable_but_not_strict(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory, repeats=3)
            summary = json.loads((directory / "summary.json").read_text())
            _fixed_static_conditions(summary, normalize_tea_generation=True)
            _legacy_v3(summary)
            (directory / "summary.json").write_text(json.dumps(summary))
            analysis = compare_run(directory)
        self.assertEqual(
            analysis["artifact_compatibility"]["legacy_migration"],
            "legacy-v3-no-post-edit-validation/v1",
        )
        self.assertTrue(analysis["comparable"])
        self.assertFalse(analysis["strict_efficiency_conclusion"])
        self.assertFalse(analysis["efficiency_gate"]["passed"])
        self.assertTrue(any("legacy v3 artifacts predate post-edit" in item for item in analysis["unknowns"]))

    def test_compare_rejects_partial_and_mixed_v3_artifacts(self) -> None:
        for name, mutate in {
            "partial": lambda summary: summary["attempts"][0]["adapter_result"].pop("validation_evidence"),
            "mixed": lambda summary: summary["attempts"][0]["adapter_result"].update({"schema_version": RESULT_SCHEMA}),
        }.items():
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                _summary(directory)
                summary = json.loads((directory / "summary.json").read_text())
                _enriched_v3(summary)
                mutate(summary)
                (directory / "summary.json").write_text(json.dumps(summary))
                with self.assertRaises(ComparisonError):
                    compare_run(directory)

    def test_model_mismatch_is_not_comparable(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory)
            summary = json.loads((directory / "summary.json").read_text())
            summary["attempts"][0]["adapter_result"]["model"]["requested_model"] = "other/model"
            (directory / "summary.json").write_text(json.dumps(summary))
            analysis = compare_run(directory)
        self.assertFalse(analysis["comparable"])
        self.assertTrue(any("model identity" in reason for reason in analysis["comparability_reasons"]))

    def test_native_prompt_and_schema_difference_remain_comparable(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory)
            summary = json.loads((directory / "summary.json").read_text())
            tea = summary["attempts"][1]["adapter_result"]
            tea["surface"]["system_prompt_sha256"] = "tea-native-prompt"
            tea["surface"]["tool_surface_sha256"] = "tea-native-tools"
            tea["wire"]["requests"][0]["system_prompt_sha256"] = "tea-native-prompt"
            tea["wire"]["requests"][0]["tool_schema_sha256"] = "tea-native-tools"
            (directory / "summary.json").write_text(json.dumps(summary))
            analysis = compare_run(directory)
        self.assertTrue(analysis["comparable"])
        self.assertTrue(analysis["comparability_checks"]["native_harness_surface_differences"])

    def test_shell_authority_difference_is_a_controlled_condition_failure(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory)
            summary = json.loads((directory / "summary.json").read_text())
            summary["attempts"][1]["adapter_result"]["surface"]["authority"]["secret_boundary"] = "different-boundary"
            (directory / "summary.json").write_text(json.dumps(summary))
            analysis = compare_run(directory)
        self.assertFalse(analysis["comparable"])
        self.assertTrue(any("shell authority" in reason for reason in analysis["comparability_reasons"]))

    def test_pre_edit_gate_difference_is_a_controlled_condition_failure(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory)
            summary = json.loads((directory / "summary.json").read_text())
            tea = summary["attempts"][1]["adapter_result"]
            direct_gate = pre_edit_tool_gate("direct-edit-v1")
            tea["surface"]["pre_edit_tool_gate"] = direct_gate
            tea["effective_policy"]["controlled"]["pre_edit_tool_gate"] = direct_gate
            (directory / "summary.json").write_text(json.dumps(summary))
            analysis = compare_run(directory)
        self.assertFalse(analysis["comparable"])
        self.assertTrue(any("pre-edit" in reason for reason in analysis["comparability_reasons"]))

    def test_pre_edit_gate_must_agree_with_run_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory)
            summary = json.loads((directory / "summary.json").read_text())
            summary["run"]["pre_edit_tool_gate"] = "direct-edit-v1"
            (directory / "summary.json").write_text(json.dumps(summary))
            analysis = compare_run(directory)
        self.assertFalse(analysis["comparable"])
        self.assertTrue(any("pre-edit" in reason for reason in analysis["comparability_reasons"]))

    def test_source_local_targets_must_agree_with_run_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory)
            summary = json.loads((directory / "summary.json").read_text())
            source_local = pre_edit_tool_gate("source-local-v1")
            for attempt in summary["attempts"]:
                result = attempt["adapter_result"]
                result["surface"]["pre_edit_tool_gate"] = source_local
                result["effective_policy"]["controlled"]["pre_edit_tool_gate"] = source_local
            summary["run"].update({
                "pre_edit_tool_gate": "source-local-v1",
                "source_local_targets": ["lib/router/index.js"],
            })
            (directory / "summary.json").write_text(json.dumps(summary))
            analysis = compare_run(directory)
        self.assertFalse(analysis["comparable"])
        self.assertTrue(any("source-local targets" in reason for reason in analysis["comparability_reasons"]))

    def test_post_edit_validation_policy_difference_is_a_controlled_condition_failure(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory)
            summary = json.loads((directory / "summary.json").read_text())
            tea = summary["attempts"][1]["adapter_result"]
            source_local = pre_edit_tool_gate("source-local-v1")
            gate = post_edit_validation_gate("unmasked-evidence-v1")
            tea["surface"]["pre_edit_tool_gate"] = source_local
            tea["effective_policy"]["controlled"]["pre_edit_tool_gate"] = source_local
            tea["surface"]["post_edit_validation_gate"] = gate
            tea["effective_policy"]["controlled"]["post_edit_validation_gate"] = gate
            tea["validation_evidence"] = validation_evidence("satisfied")
            tea["trace"].extend(validation_transitions("satisfied"))
            tea["harness"].update({"mode": "static", "decision": "not-applicable"})
            (directory / "summary.json").write_text(json.dumps(summary))
            analysis = compare_run(directory)
        self.assertFalse(analysis["comparable"])
        self.assertTrue(any("post-edit" in reason for reason in analysis["comparability_reasons"]))

    def test_post_edit_validation_policy_must_agree_with_run_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory)
            summary = json.loads((directory / "summary.json").read_text())
            summary["run"]["post_edit_validation_gate"] = "unmasked-evidence-v1"
            (directory / "summary.json").write_text(json.dumps(summary))
            analysis = compare_run(directory)
        self.assertFalse(analysis["comparable"])
        self.assertTrue(any("post-edit" in reason for reason in analysis["comparability_reasons"]))

    def test_post_edit_validation_outcomes_do_not_require_adapter_identity(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory)
            summary = json.loads((directory / "summary.json").read_text())
            source_local = pre_edit_tool_gate("source-local-v1")
            gate = post_edit_validation_gate("unmasked-evidence-v1")
            for ordinal, attempt in enumerate(summary["attempts"]):
                result = attempt["adapter_result"]
                result["surface"]["pre_edit_tool_gate"] = source_local
                result["effective_policy"]["controlled"]["pre_edit_tool_gate"] = source_local
                result["surface"]["post_edit_validation_gate"] = gate
                result["effective_policy"]["controlled"]["post_edit_validation_gate"] = gate
                state = "satisfied" if ordinal == 0 else "missing"
                result["validation_evidence"] = validation_evidence(state)
                result["trace"].extend(validation_transitions(state))
                result["harness"].update({"mode": "static", "decision": "not-applicable"})
            summary["run"].update({
                "pre_edit_tool_gate": "source-local-v1",
                "post_edit_validation_gate": "unmasked-evidence-v1",
                "source_local_targets": ["lib/response.js"],
            })
            (directory / "summary.json").write_text(json.dumps(summary))
            analysis = compare_run(directory)
        self.assertTrue(analysis["comparable"])

    def test_equal_null_controlled_policy_values_are_not_unobserved(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory)
            summary = json.loads((directory / "summary.json").read_text())
            for attempt in summary["attempts"]:
                result = attempt["adapter_result"]
                result["effective_policy"]["controlled"]["request_timeout_seconds"] = 1800
                result["effective_policy"]["controlled"]["idle_timeout_seconds"] = 1800
                result["wire"]["returned_route"] = {"model": "deepseek-v4-flash", "provider": "deepseek", "provenance": "header"}
                result["model"]["sampling"] = {"temperature": 0, "seed": 20260829, "source": "adapter-set"}
                result["effective_policy"]["controlled"]["sampling"] = {"temperature": 0, "seed": 20260829}
                request = result["wire"]["requests"][0]
                request["temperature"] = {"present": True, "value": 0}
                request["seed"] = {"present": True, "value": 20260829}
            (directory / "summary.json").write_text(json.dumps(summary))
            analysis = compare_run(directory)
        self.assertTrue(analysis["strict_efficiency_conclusion"])

    def test_wire_mutations_fail_the_correct_integrity_check(self) -> None:
        mutations = {
            "pi_tools_empty": lambda pi, tea: pi["wire"]["requests"][0].update({"tool_count": 0, "tool_names": []}),
            "tea_temperature_zero": lambda pi, tea: tea["wire"]["requests"][0]["temperature"].update({"present": True, "value": 0}),
            "routing_mismatch": lambda pi, tea: tea["wire"]["requests"][0].update({"provider_routing": {"require_parameters": False}}),
            "reasoning_content_missing": lambda pi, tea: pi["wire"]["requests"][0].update({"assistant_reasoning_content": False}),
            "extra_tool": lambda pi, tea: tea["wire"]["requests"][0].update({"tool_count": 5, "tool_names": ["read", "bash", "edit", "find", "web"]}),
        }
        for name, mutate in mutations.items():
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                _summary(directory)
                summary = json.loads((directory / "summary.json").read_text())
                pi, tea = summary["attempts"][0]["adapter_result"], summary["attempts"][1]["adapter_result"]
                mutate(pi, tea)
                (directory / "summary.json").write_text(json.dumps(summary))
                analysis = compare_run(directory)
                self.assertFalse(analysis["comparable"])
                self.assertTrue(analysis["comparability_checks"]["wire_shape_bugs"] or analysis["comparability_checks"]["controlled_condition_mismatches"])

    def test_unlinked_durable_requests_are_marked_incomplete_not_positionally_paired(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory)
            analysis = compare_run(directory)
        tea_turns = analysis["pairs"][0]["tea"]["trace"]["turn_evidence"]
        self.assertFalse(tea_turns["complete"])
        self.assertTrue(all(turn["provider_request"] is None for turn in tea_turns["turns"]))

    def test_repeated_pairs_report_median_and_worst_case(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory, repeats=2)
            analysis = compare_run(directory)
        generation = analysis["aggregate_delta_tea_minus_pi"]["usage"]["generation"]
        self.assertEqual(generation["samples"], 2)
        self.assertEqual(generation["median"], 2)
        self.assertEqual(generation["max"], 2)
        self.assertEqual(analysis["pairs"][1]["tea"]["counts"]["provider_requests"], None)
        self.assertFalse(analysis["efficiency_gate"]["passed"])
        self.assertIn(
            "worst-case Tea-minus-Pi uncached generation is greater than zero",
            analysis["efficiency_gate"]["reasons"],
        )

    def test_static_efficiency_gate_requires_three_comparable_pairs_and_non_positive_worst_case(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory, repeats=3)
            summary = json.loads((directory / "summary.json").read_text())
            for attempt in summary["attempts"]:
                result = attempt["adapter_result"]
                result["model"]["sampling"] = {"temperature": 0, "seed": 20260829, "source": "adapter-set"}
                result["effective_policy"]["controlled"]["sampling"] = {"temperature": 0, "seed": 20260829}
                result["effective_policy"]["controlled"]["request_timeout_seconds"] = 1800
                result["effective_policy"]["controlled"]["idle_timeout_seconds"] = 1800
                result["wire"]["returned_route"] = {"model": "deepseek-v4-flash", "provider": "deepseek", "provenance": "header"}
                request = result["wire"]["requests"][0]
                request["temperature"] = {"present": True, "value": 0}
                request["seed"] = {"present": True, "value": 20260829}
                if attempt["baseline_id"] == "tea-static":
                    result["usage"]["input"] -= 2
                    result["usage"]["prompt_total"] -= 2
                    result["usage"]["generation"] -= 2
                    result["usage"]["all_tokens"] -= 2
            (directory / "summary.json").write_text(json.dumps(summary))
            analysis = compare_run(directory)
        self.assertTrue(
            analysis["efficiency_gate"]["passed"],
            {"gate": analysis["efficiency_gate"], "checks": analysis["comparability_checks"]},
        )
        self.assertEqual(analysis["efficiency_gate"]["generation_delta"]["worst_case"], 0)

    def test_writes_machine_and_markdown_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory)
            analysis = compare_run(directory)
            output, markdown = write_comparison(analysis, directory / "reports" / "comparison.json", directory / "reports" / "comparison.md")
            self.assertEqual(json.loads(output.read_text())["schema_version"], "tea-pi-shootout-analysis/v2")
            self.assertIn("Turn evidence", markdown.read_text())
            self.assertIn("Static paired efficiency gate", markdown.read_text())
            self.assertIn("generation", render_markdown(analysis))


if __name__ == "__main__":
    unittest.main()
