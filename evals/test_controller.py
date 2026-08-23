from __future__ import annotations

import copy
import os
from pathlib import Path
import shutil
import sys
import tempfile
import threading
import time
import unittest

from . import controller


class ControllerContractTests(unittest.TestCase):
    def test_checked_in_tasks_validate(self) -> None:
        tasks = controller.load_tasks(controller.TASKS)
        self.assertEqual([task["task_id"] for task in tasks], ["interval-merge-v1", "ready-v1"])

    def test_example_baselines_validate_and_plan_is_deterministic(self) -> None:
        config = controller.load_baselines(controller.ROOT / "baselines.example.json")
        tasks = controller.load_tasks(controller.TASKS)
        first = controller.paired_plan(tasks, config)
        second = controller.paired_plan(tasks, config)
        self.assertEqual(first, second)
        self.assertEqual({item["baseline_id"] for item in first}, {"upstream", "rust"})

    def test_task_selection_is_explicit_and_rejects_unknown_ids(self) -> None:
        tasks = controller.load_tasks(controller.TASKS)
        self.assertEqual([task["task_id"] for task in controller.select_tasks(tasks, ["ready-v1"])], ["ready-v1"])
        with self.assertRaises(controller.ContractError):
            controller.select_tasks(tasks, ["not-a-task"])

    def test_interval_task_declares_the_exact_active_profile_tool_schemas(self) -> None:
        task = controller.load_tasks(controller.TASKS)[0]
        profile = controller.read_json(controller.ROOT.parent / "crates" / "tea-core" / "profile" / "default-profile.json")
        task_schemas = {capability["name"]: capability["schema"] for capability in task["capabilities"]}
        profile_schemas = {tool["name"]: tool["parameters"] for tool in profile["active_tools"]}
        self.assertEqual(task_schemas, profile_schemas)

    def test_baseline_manifest_requires_explicit_versioned_adapter_contract(self) -> None:
        config = controller.load_baselines(controller.ROOT / "baselines.example.json")
        missing_adapter = copy.deepcopy(config)
        del missing_adapter["baselines"][0]["adapter"]
        with self.assertRaises(controller.ContractError):
            controller.validate_baselines(missing_adapter)

        missing_identity = copy.deepcopy(config)
        missing_identity["baselines"][0]["command"].remove("{attempt_id}")
        with self.assertRaises(controller.ContractError):
            controller.validate_baselines(missing_identity)

        host_cli = copy.deepcopy(config)
        host_cli["baselines"][0]["command"][0] = "pi"
        with self.assertRaises(controller.ContractError):
            controller.validate_baselines(host_cli)

    def test_mock_manifest_runs_both_baselines_and_tasks_without_provider(self) -> None:
        output_name = "mock-report.json"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            completed = controller.subprocess.run(
                [
                    sys.executable,
                    str(controller.ROOT / "controller.py"),
                    "run",
                    "--baselines",
                    str(controller.ROOT / "baselines.mock.json"),
                    "--allow-provider",
                    "--workspace-root",
                    str(root / "workspaces"),
                    "--out",
                    str(root / output_name),
                ],
                cwd=controller.ROOT.parent,
                env=controller.safe_environment(),
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            report = controller.read_json(root / output_name)
            self.assertEqual(len(report["records"]), 4)
            self.assertTrue(all(record["status"] == "success" for record in report["records"]))
            self.assertEqual(report["summary"]["upstream"]["successes"], 2)
            self.assertEqual(report["summary"]["rust"]["successes"], 2)

    def test_workspace_parent_and_initial_paths_are_explicit(self) -> None:
        task = copy.deepcopy(controller.load_tasks(controller.TASKS)[0])
        task["initial_workspace"] = [{"path": "input/value.txt", "content": "fixed\n"}]
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary)
            workspace = controller.materialize_workspace(task, parent)
            try:
                self.assertEqual((workspace / "input/value.txt").read_text(), "fixed\n")
                self.assertEqual(workspace.parent, parent.resolve())
            finally:
                shutil.rmtree(workspace)

    def test_parent_and_absolute_paths_are_rejected(self) -> None:
        for path in ("../escape.txt", "/tmp/escape.txt", "a\\b.txt", ""):
            with self.subTest(path=path):
                with self.assertRaises(controller.ContractError):
                    controller.relative_path(path)

    def test_symlink_workspace_parent_is_rejected(self) -> None:
        task = controller.load_tasks(controller.TASKS)[0]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / "target"
            target.mkdir()
            link = root / "link"
            os.symlink(target, link)
            with self.assertRaises(controller.ContractError):
                controller.materialize_workspace(task, link)

    def test_run_requires_explicit_provider_opt_in(self) -> None:
        task = controller.load_tasks(controller.TASKS)[0]
        config = controller.load_baselines(controller.ROOT / "baselines.example.json")
        baseline = config["baselines"][0]
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaises(controller.ContractError):
                controller.run_attempt(
                    task,
                    baseline,
                    attempt_id="ready-v1-r0-upstream",
                    workspace_parent=Path(temporary),
                    allow_provider=False,
                    comparison=config["comparison"],
                )
            self.assertEqual(list(Path(temporary).iterdir()), [])

    def test_local_adapter_result_is_controller_scored(self) -> None:
        task = controller.load_tasks(controller.TASKS)[1]
        config = controller.load_baselines(controller.ROOT / "baselines.example.json")
        baseline = copy.deepcopy(config["baselines"][0])
        baseline["command"] = [
            sys.executable,
            "-c",
            "import json,sys; json.dump(dict(schema_version='tea-coding-eval-result/v1', attempt_id=sys.argv[2], baseline_id=sys.argv[3], terminal=dict(status='completed'), final_text='READY', turns=0, tool_calls=0, usage=dict(input=0, output=0, cache_read=0, cache_write=0), trace=[]), open(sys.argv[1], 'w'))",
            "{result_json}",
            "{attempt_id}",
            "{baseline_id}",
        ]
        with tempfile.TemporaryDirectory() as temporary:
            record = controller.run_attempt(
                task,
                baseline,
                attempt_id="ready-v1-r0-upstream",
                workspace_parent=Path(temporary),
                allow_provider=True,
                comparison=config["comparison"],
            )
            self.assertEqual(record["status"], "success")
            self.assertEqual(record["oracle"]["status"], "passed")
            self.assertEqual(record["terminal_status"], "completed")

    def test_controller_does_not_forward_secret_environment(self) -> None:
        environment = controller.safe_environment()
        self.assertNotIn("OPENROUTER_API_KEY", environment)
        self.assertNotIn("ANTHROPIC_API_KEY", environment)

    def test_adapter_timeout_terminates_the_entire_process_group(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            sentinel = root / "orphaned-child.txt"
            child_code = (
                "import pathlib,time; time.sleep(2); "
                f"pathlib.Path({str(sentinel)!r}).write_text('leaked')"
            )
            parent_code = (
                "import subprocess,sys,time; "
                f"subprocess.Popen([sys.executable, '-c', {child_code!r}]); "
                "time.sleep(60)"
            )
            completed, timed_out = controller.run_adapter_process(
                [sys.executable, "-c", parent_code],
                cwd=root,
                environment=controller.safe_environment(),
                timeout_seconds=1,
            )

            self.assertTrue(timed_out)
            self.assertNotEqual(completed.returncode, 0)
            time.sleep(2.2)
            self.assertFalse(sentinel.exists(), "adapter child survived controller timeout")

    def test_hidden_interval_oracle_scores_workspace_not_final_text(self) -> None:
        task = controller.load_tasks(controller.TASKS)[0]
        passing_result = {"terminal": {"status": "completed"}, "final_text": "anything"}
        with tempfile.TemporaryDirectory() as temporary:
            workspace = Path(temporary)
            (workspace / "intervals.py").write_text(
                "def merge_intervals(values):\n"
                "    values = sorted(values)\n"
                "    output = []\n"
                "    for start, end in values:\n"
                "        if start > end:\n"
                "            raise ValueError()\n"
                "        if output and start <= output[-1][1] + 1:\n"
                "            output[-1] = (output[-1][0], max(output[-1][1], end))\n"
                "        else:\n"
                "            output.append((start, end))\n"
                "    return output\n"
            )
            result = controller.verify_result(task, workspace, passing_result)
            self.assertEqual(result["status"], "passed")
            (workspace / "intervals.py").write_text("def merge_intervals(values): return values\n")
            result = controller.verify_result(task, workspace, passing_result)
            self.assertEqual(result["status"], "failed")

    def test_controller_only_accepts_supported_result_shape(self) -> None:
        task = controller.load_tasks(controller.TASKS)[1]
        with tempfile.TemporaryDirectory() as temporary:
            result = controller.verify_result(
                task,
                Path(temporary),
                {"terminal": {"status": "completed"}, "final_text": "READY"},
            )
            self.assertEqual(result["status"], "passed")

    def test_adapter_result_identity_is_required_when_validating_directly(self) -> None:
        with self.assertRaises(controller.ContractError):
            controller.validate_adapter_result({"terminal": {"status": "completed"}})
        with self.assertRaises(controller.ContractError):
            controller.validate_adapter_result(
                {
                    "schema_version": controller.RESULT_SCHEMA,
                    "attempt_id": "other",
                    "baseline_id": "upstream",
                    "terminal": {"status": "completed"},
                    "final_text": "READY",
                    "turns": 0,
                    "tool_calls": 0,
                    "usage": {"input": 0, "output": 0, "cache_read": 0, "cache_write": 0},
                    "trace": [],
                },
                attempt_id="expected",
                baseline_id="upstream",
            )

    def test_adapter_provider_error_retains_only_safe_classification(self) -> None:
        result = {
            "schema_version": controller.RESULT_SCHEMA,
            "attempt_id": "attempt",
            "baseline_id": "rust",
            "terminal": {"status": "failed"},
            "final_text": "",
            "turns": 1,
            "tool_calls": 0,
            "usage": {"input": 0, "output": 0, "cache_read": 0, "cache_write": 0},
            "trace": [],
            "provider_error": {
                "source": "gateway",
                "status_code": 429,
                "error_type": "rate_limit",
                "error_code": "upstream_failed",
                "retryable": True,
            },
        }
        controller.validate_adapter_result(result, attempt_id="attempt", baseline_id="rust")
        result["provider_error"]["message"] = "arbitrary remote payload"
        with self.assertRaises(controller.ContractError):
            controller.validate_adapter_result(result, attempt_id="attempt", baseline_id="rust")
        del result["provider_error"]["message"]
        result["provider_error"]["status_code"] = 99
        with self.assertRaises(controller.ContractError):
            controller.validate_adapter_result(result, attempt_id="attempt", baseline_id="rust")

    def test_summary_reports_success_rate_and_latency_quantiles(self) -> None:
        records = [
            {"baseline_id": "rust", "status": "success", "elapsed_ms": 10, "turns": 1, "tool_calls": 2, "usage": {"input": 3, "output": 4, "cache_read": 0, "cache_write": 0}},
            {"baseline_id": "rust", "status": "failure", "elapsed_ms": 20, "turns": 2, "tool_calls": 3, "usage": {"input": 5, "output": 6, "cache_read": 0, "cache_write": 0}},
        ]
        summary = controller.summarize(records)["rust"]
        self.assertEqual(summary["attempts"], 2)
        self.assertEqual(summary["successes"], 1)
        self.assertEqual(summary["elapsed_ms_median"], 15.0)
        self.assertEqual(summary["tokens_median"], 9.0)
        self.assertIsNotNone(summary["success_rate_95ci"])

    def test_paired_cost_comparison_refuses_partial_accounting(self) -> None:
        complete = controller.paired_cost_comparison({
            "upstream": {"provider_reported_cost_usd": {"total": 0.25, "incomplete_or_unreported_attempts": 0}},
            "rust": {"provider_reported_cost_usd": {"total": 0.30, "incomplete_or_unreported_attempts": 0}},
        })
        self.assertTrue(complete["complete"])
        self.assertAlmostEqual(complete["rust_minus_upstream_usd"], 0.05)
        partial = controller.paired_cost_comparison({
            "upstream": {"provider_reported_cost_usd": {"total": 0.25, "incomplete_or_unreported_attempts": 1}},
            "rust": {"provider_reported_cost_usd": {"total": 0.30, "incomplete_or_unreported_attempts": 0}},
        })
        self.assertFalse(partial["complete"])

    def test_wave_controller_honors_admission_and_reports_peak(self) -> None:
        config = controller.load_baselines(controller.ROOT / "baselines.example.json")
        config["waves"]["ready"].update({
            "concurrency": 3,
            "admission_concurrency": 2,
            "stagger_ms": 0,
            "stop_on_failure": False,
        })
        task = controller.load_tasks(controller.TASKS)[1]
        baseline = config["baselines"][0]
        items = [
            {"repeat": repeat, "task_id": task["task_id"], "baseline_id": baseline["id"], "wave": "ready"}
            for repeat in range(3)
        ]
        active, peak = 0, 0
        lock = threading.Lock()

        def fake_run_attempt(_task, _baseline, *, attempt_id, **_kwargs):
            nonlocal active, peak
            with lock:
                active += 1
                peak = max(peak, active)
            try:
                time.sleep(0.01)
                return {"attempt_id": attempt_id, "status": "success"}
            finally:
                with lock:
                    active -= 1

        with tempfile.TemporaryDirectory() as temporary:
            records, report = controller.execute_wave(
                items,
                {task["task_id"]: task},
                {baseline["id"]: baseline},
                wave_name="ready",
                wave=config["waves"]["ready"],
                workspace_parent=Path(temporary),
                comparison=config["comparison"],
                run_attempt_fn=fake_run_attempt,
            )

        self.assertEqual([record["attempt_id"] for record in records], [
            "ready-v1-r0-upstream", "ready-v1-r1-upstream", "ready-v1-r2-upstream",
        ])
        self.assertEqual(report["logical_concurrency"], 3)
        self.assertEqual(report["admission_concurrency"], 2)
        self.assertEqual(report["observed_active_peak"], 2)
        self.assertEqual(peak, 2)


if __name__ == "__main__":
    unittest.main()
