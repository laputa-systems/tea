from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from .compare import compare_run, render_markdown, write_comparison
from .runner import DEFAULT_MODEL, DEFAULT_THINKING
from .test_contract import result as valid_result


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
    return {
        "baseline_id": baseline,
        "attempt_id": attempt_id,
        "adapter_result": value,
        "validator": {"passed": True},
        "process": {"peak_rss_bytes": None},
        "timings": {"total_attempt_ms": 1},
        "patch_sha256": "patch",
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


class CompareTest(unittest.TestCase):
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

    def test_surface_or_model_mismatch_is_not_comparable(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory)
            summary = json.loads((directory / "summary.json").read_text())
            summary["attempts"][0]["adapter_result"]["model"]["requested_model"] = "other/model"
            (directory / "summary.json").write_text(json.dumps(summary))
            analysis = compare_run(directory)
        self.assertFalse(analysis["comparable"])
        self.assertTrue(any("model identity" in reason for reason in analysis["comparability_reasons"]))

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

    def test_writes_machine_and_markdown_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            _summary(directory)
            analysis = compare_run(directory)
            output, markdown = write_comparison(analysis, directory / "reports" / "comparison.json", directory / "reports" / "comparison.md")
            self.assertEqual(json.loads(output.read_text())["schema_version"], "tea-pi-shootout-analysis/v1")
            self.assertIn("Turn evidence", markdown.read_text())
            self.assertIn("generation", render_markdown(analysis))


if __name__ == "__main__":
    unittest.main()
