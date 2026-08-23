#!/usr/bin/env python3
"""Stdlib-only, provider-opt-in controller for the v1 coding evaluation.

Validation and planning are inert. A live run requires --allow-provider and an explicit argv
adapter; the controller never discovers or forwards credentials and never uses a shell.
"""

from __future__ import annotations

import argparse
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
import hashlib
import json
import math
import os
from pathlib import Path, PurePosixPath
import random
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
from threading import Lock
import time
from typing import Any, Iterable

ROOT = Path(__file__).resolve().parent
TASKS = ROOT / "tasks"
TASK_SCHEMA = "tea-coding-eval-task/v1"
BASELINE_SCHEMA = "tea-coding-eval-baselines/v1"
RESULT_SCHEMA = "tea-coding-eval-result/v1"
ADAPTER_SCHEMA = "tea-coding-eval-adapter/v1"
# These are the complete adapter argv contract.  Identity values are passed explicitly so
# adapters never infer them from temporary filenames or hard-code a single attempt.
TOKENS = (
    "{task_json}",
    "{workspace}",
    "{result_json}",
    "{capabilities_json}",
    "{attempt_id}",
    "{baseline_id}",
)
OPTIONAL_TOKENS = ("{controller_root}", "{controller_python}")
ID = re.compile(r"^[a-z0-9][a-z0-9-]{0,63}$")
HOST_PI_EXECUTABLES = {"pi", "tea", "tea-core"}


class ContractError(ValueError):
    """A task or baseline contract is invalid."""


def canonical(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")


def digest(value: Any) -> str:
    return hashlib.sha256(canonical(value)).hexdigest()


def read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise ContractError(f"cannot read JSON contract {path}: {exc}") from exc
    if not isinstance(value, dict):
        raise ContractError(f"{path} must contain an object")
    return value


def required_string(obj: dict[str, Any], name: str, *, allow_empty: bool = False) -> str:
    value = obj.get(name)
    if not isinstance(value, str) or (not allow_empty and not value):
        raise ContractError(f"{name} must be a non-empty string")
    return value


def relative_path(value: str) -> PurePosixPath:
    if not isinstance(value, str) or not value or "\x00" in value or "\\" in value:
        raise ContractError(f"workspace path is not a safe relative path: {value!r}")
    path = PurePosixPath(value)
    if path.is_absolute() or any(part in ("", ".", "..") for part in path.parts):
        raise ContractError(f"workspace path is not a safe relative path: {value!r}")
    return path


def validate_task(task: dict[str, Any], source: str = "task") -> dict[str, Any]:
    if task.get("schema_version") != TASK_SCHEMA:
        raise ContractError(f"{source}: schema_version must be {TASK_SCHEMA!r}")
    task_id = required_string(task, "task_id")
    if not ID.fullmatch(task_id):
        raise ContractError(f"{source}: task_id must be lowercase kebab-case")
    if not isinstance(task.get("task_version"), int) or task["task_version"] < 1:
        raise ContractError(f"{source}: task_version must be positive")
    if task.get("kind") not in ("control", "coding"):
        raise ContractError(f"{source}: kind must be control or coding")
    required_string(task, "prompt")
    workspace = task.get("initial_workspace")
    if not isinstance(workspace, list):
        raise ContractError(f"{source}: initial_workspace must be an array")
    seen_paths: set[str] = set()
    for index, item in enumerate(workspace):
        if not isinstance(item, dict):
            raise ContractError(f"{source}: initial_workspace[{index}] must be an object")
        path = required_string(item, "path")
        relative_path(path)
        if path in seen_paths:
            raise ContractError(f"{source}: duplicate initial workspace path {path!r}")
        seen_paths.add(path)
        required_string(item, "content", allow_empty=True)
    capabilities = task.get("capabilities")
    if not isinstance(capabilities, list):
        raise ContractError(f"{source}: capabilities must be an array")
    seen_capabilities: set[str] = set()
    for index, item in enumerate(capabilities):
        if not isinstance(item, dict):
            raise ContractError(f"{source}: capabilities[{index}] must be an object")
        name = required_string(item, "name")
        if not ID.fullmatch(name) or name in seen_capabilities:
            raise ContractError(f"{source}: invalid or duplicate capability {name!r}")
        seen_capabilities.add(name)
        required_string(item, "kind")
        if not isinstance(item.get("schema"), dict):
            raise ContractError(f"{source}: capability {name!r} needs a schema object")
    timeout = task.get("timeout_seconds")
    if not isinstance(timeout, int) or not 1 <= timeout <= 86_400:
        raise ContractError(f"{source}: timeout_seconds must be 1..86400")
    oracle = required_string(task, "oracle_id")
    if not ID.fullmatch(oracle):
        raise ContractError(f"{source}: oracle_id must be lowercase kebab-case")
    return task


def load_tasks(path: Path = TASKS) -> list[dict[str, Any]]:
    files = sorted(path.glob("*.json"))
    if not files:
        raise ContractError(f"no task contracts found under {path}")
    return [validate_task(read_json(file), str(file)) for file in files]


def select_tasks(tasks: list[dict[str, Any]], requested: Iterable[str] | None) -> list[dict[str, Any]]:
    """Select named task contracts without changing their on-disk source of truth."""
    ids = set(requested or ())
    if not ids:
        return tasks
    known = {task["task_id"] for task in tasks}
    missing = ids - known
    if missing:
        raise ContractError(f"unknown task id(s): {', '.join(sorted(missing))}")
    return [task for task in tasks if task["task_id"] in ids]


def validate_wave(wave: Any, name: str) -> None:
    if not isinstance(wave, dict):
        raise ContractError(f"wave {name!r} must be an object")
    for key in ("concurrency", "admission_concurrency"):
        if not isinstance(wave.get(key), int) or wave[key] < 1:
            raise ContractError(f"wave {name!r}: {key} must be positive")
    if not isinstance(wave.get("stagger_ms"), int) or wave["stagger_ms"] < 0:
        raise ContractError(f"wave {name!r}: stagger_ms must be non-negative")
    if not isinstance(wave.get("stop_on_failure"), bool):
        raise ContractError(f"wave {name!r}: stop_on_failure must be boolean")


def validate_baselines(config: dict[str, Any], source: str = "baselines") -> dict[str, Any]:
    if config.get("schema_version") != BASELINE_SCHEMA:
        raise ContractError(f"{source}: schema_version must be {BASELINE_SCHEMA!r}")
    if not isinstance(config.get("seed"), int) or not isinstance(config.get("repeats"), int) or config["repeats"] < 1:
        raise ContractError(f"{source}: seed must be integer and repeats must be positive")
    comparison = config.get("comparison")
    if not isinstance(comparison, dict):
        raise ContractError(f"{source}: comparison must be an object")
    for key in ("model", "provider_revision"):
        required_string(comparison, key)
    if not isinstance(comparison.get("sampling"), dict):
        raise ContractError(f"{source}: comparison.sampling must be an object")
    if not isinstance(comparison.get("timeout_seconds"), int) or comparison["timeout_seconds"] < 1:
        raise ContractError(f"{source}: comparison.timeout_seconds must be positive")
    waves = config.get("waves")
    if not isinstance(waves, dict) or not waves:
        raise ContractError(f"{source}: waves must be non-empty")
    for name, wave in waves.items():
        validate_wave(wave, name)
    baselines = config.get("baselines")
    if not isinstance(baselines, list) or len(baselines) < 2:
        raise ContractError(f"{source}: at least two baselines are required")
    ids: set[str] = set()
    for index, baseline in enumerate(baselines):
        if not isinstance(baseline, dict):
            raise ContractError(f"{source}: baselines[{index}] must be an object")
        baseline_id = required_string(baseline, "id")
        if not ID.fullmatch(baseline_id) or baseline_id in ids:
            raise ContractError(f"{source}: invalid or duplicate baseline id {baseline_id!r}")
        ids.add(baseline_id)
        for key in ("label", "profile_version", "runtime_version", "revision"):
            required_string(baseline, key)
        adapter = baseline.get("adapter")
        if not isinstance(adapter, dict):
            raise ContractError(f"{source}: baseline {baseline_id!r} needs an adapter object")
        if adapter.get("protocol") != ADAPTER_SCHEMA:
            raise ContractError(
                f"{source}: baseline {baseline_id!r} adapter.protocol must be {ADAPTER_SCHEMA!r}"
            )
        if adapter.get("result_schema") != RESULT_SCHEMA:
            raise ContractError(
                f"{source}: baseline {baseline_id!r} adapter.result_schema must be {RESULT_SCHEMA!r}"
            )
        command = baseline.get("command")
        if not isinstance(command, list) or not command or any(
            not isinstance(part, str) or not part or "\x00" in part for part in command
        ):
            raise ContractError(f"{source}: baseline {baseline_id!r} command must be argv")
        executable = Path(command[0]).name
        if executable in HOST_PI_EXECUTABLES:
            raise ContractError(
                f"{source}: baseline {baseline_id!r} must name an explicit adapter, not host Pi executable {executable!r}"
            )
        for token in TOKENS:
            occurrences = sum(part.count(token) for part in command)
            if occurrences != 1:
                raise ContractError(
                    f"{source}: baseline {baseline_id!r} must contain {token} exactly once (found {occurrences})"
                )
        known_tokens = set(TOKENS) | set(OPTIONAL_TOKENS)
        for part in command:
            unresolved = re.findall(r"\{[^{}]+\}", part)
            unknown = [token for token in unresolved if token not in known_tokens]
            if unknown:
                raise ContractError(
                    f"{source}: baseline {baseline_id!r} has unsupported command placeholders {unknown}"
                )
    if not {"upstream", "rust"}.issubset(ids):
        raise ContractError(f"{source}: baselines must include both 'upstream' and 'rust'")
    return config


def load_baselines(path: Path) -> dict[str, Any]:
    return validate_baselines(read_json(path), str(path))


def workspace_child(root: Path, value: str) -> Path:
    safe = relative_path(value)
    resolved_root = root.resolve()
    candidate = (resolved_root / Path(*safe.parts)).resolve()
    try:
        candidate.relative_to(resolved_root)
    except ValueError as exc:
        raise ContractError(f"workspace path escapes root: {value!r}") from exc
    return candidate


def assert_no_symlink_ancestors(path: Path) -> None:
    absolute = Path(os.path.abspath(path))
    current = Path(absolute.anchor)
    for part in absolute.parts[1:]:
        current /= part
        if current.is_symlink() and current.resolve() not in {
            Path("/private/var"),
            Path("/private/tmp"),
        }:
            raise ContractError(f"workspace parent contains symlink: {current}")


def materialize_workspace(task: dict[str, Any], parent: Path) -> Path:
    assert_no_symlink_ancestors(parent)
    parent.mkdir(parents=True, exist_ok=True)
    parent = parent.resolve()
    root = Path(tempfile.mkdtemp(prefix=f"tea-eval-{task['task_id']}-", dir=parent))
    try:
        for item in task["initial_workspace"]:
            destination = workspace_child(root, item["path"])
            destination.parent.mkdir(parents=True, exist_ok=True)
            if destination.exists() or destination.is_symlink():
                raise ContractError(f"initial path already exists: {item['path']!r}")
            destination.write_text(item["content"], encoding="utf-8")
    except BaseException:
        shutil.rmtree(root, ignore_errors=True)
        raise
    return root


def render_command(command: Iterable[str], replacements: dict[str, str]) -> list[str]:
    output: list[str] = []
    for part in command:
        rendered = part
        for token, value in replacements.items():
            rendered = rendered.replace(token, value)
        if "{" in rendered or "}" in rendered:
            raise ContractError(f"unsupported command placeholder: {part!r}")
        output.append(rendered)
    return output


def safe_environment() -> dict[str, str]:
    """Only ordinary process settings are passed; no parent secret is forwarded."""
    return {key: value for key, value in {
        "PATH": os.environ.get("PATH", ""),
        "LANG": "C",
        "LC_ALL": "C",
    }.items() if value}


def run_adapter_process(
    command: list[str], *, cwd: Path, environment: dict[str, str], timeout_seconds: int
) -> tuple[subprocess.CompletedProcess[str], bool]:
    """Run one adapter in its own process group and reap every child on timeout.

    Live adapter commands commonly contain a secret injector followed by a wrapper and a model
    process. Killing only the injector can orphan the provider client, leaving it to consume
    resources after the controller recorded a timeout. The group is a local cleanup boundary; it
    does not grant the controller authority beyond the adapter process it started.
    """

    process = subprocess.Popen(
        command,
        cwd=cwd,
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=os.name == "posix",
    )
    try:
        stdout, stderr = process.communicate(timeout=timeout_seconds)
        return subprocess.CompletedProcess(command, process.returncode, stdout, stderr), False
    except subprocess.TimeoutExpired:
        if os.name == "posix":
            os.killpg(process.pid, signal.SIGTERM)
        else:
            process.terminate()
        try:
            stdout, stderr = process.communicate(timeout=2)
        except subprocess.TimeoutExpired:
            if os.name == "posix":
                os.killpg(process.pid, signal.SIGKILL)
            else:
                process.kill()
            stdout, stderr = process.communicate()
        return subprocess.CompletedProcess(command, process.returncode, stdout, stderr), True


def verify_ready(workspace: Path, result: dict[str, Any]) -> dict[str, Any]:
    del workspace
    passed = result.get("final_text") == "READY" and result.get("terminal", {}).get("status") == "completed"
    return {"oracle_id": "ready-exact-v1", "status": "passed" if passed else "failed"}


INTERVAL_ORACLE = r'''
import sys
sys.path.insert(0, ".")
from intervals import merge_intervals

def check(actual, expected):
    assert actual == expected, (actual, expected)

original = [(5, 7), (1, 2), (3, 4), (10, 12), (11, 15)]
snapshot = list(original)
check(merge_intervals(original), [(1, 7), (10, 15)])
assert original == snapshot
check(merge_intervals([(1, 10), (2, 6), (8, 12)]), [(1, 12)])
check(merge_intervals([(1, 2), (3, 4)]), [(1, 4)])
check(merge_intervals([(1, 2), (4, 5)]), [(1, 2), (4, 5)])
check(merge_intervals([(-10, -8), (-5, -1)]), [(-10, -8), (-5, -1)])
check(merge_intervals([]), [])
try:
    merge_intervals([(2, 1)])
except ValueError:
    pass
else:
    raise AssertionError("invalid interval did not raise")
'''


def verify_interval(workspace: Path, result: dict[str, Any]) -> dict[str, Any]:
    if result.get("terminal", {}).get("status") != "completed":
        return {"oracle_id": "interval-merge-hidden-v1", "status": "failed", "detail": "not completed"}
    implementation = workspace / "intervals.py"
    if not implementation.is_file() or implementation.is_symlink():
        return {"oracle_id": "interval-merge-hidden-v1", "status": "failed", "detail": "intervals.py missing"}
    try:
        completed = subprocess.run(
            [sys.executable, "-I", "-c", INTERVAL_ORACLE],
            cwd=workspace, env=safe_environment(), capture_output=True, text=True, timeout=10, check=False
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return {"oracle_id": "interval-merge-hidden-v1", "status": "failed", "detail": str(exc)}
    if completed.returncode == 0:
        return {"oracle_id": "interval-merge-hidden-v1", "status": "passed"}
    detail = (completed.stdout + "\n" + completed.stderr).strip()[-2_000:]
    return {"oracle_id": "interval-merge-hidden-v1", "status": "failed", "detail": detail}


def verify_result(task: dict[str, Any], workspace: Path, result: dict[str, Any]) -> dict[str, Any]:
    oracle = task["oracle_id"]
    if oracle == "ready-exact-v1":
        return verify_ready(workspace, result)
    if oracle == "interval-merge-hidden-v1":
        return verify_interval(workspace, result)
    raise ContractError(f"no controller-owned oracle for {oracle!r}")


def validate_adapter_result(
    result: dict[str, Any], *, attempt_id: str | None = None, baseline_id: str | None = None
) -> None:
    if result.get("schema_version") != RESULT_SCHEMA:
        raise ContractError(f"adapter result schema must be {RESULT_SCHEMA!r}")
    if attempt_id is not None and result.get("attempt_id") != attempt_id:
        raise ContractError("adapter result attempt_id does not match")
    if baseline_id is not None and result.get("baseline_id") != baseline_id:
        raise ContractError("adapter result baseline_id does not match")
    terminal = result.get("terminal")
    if not isinstance(terminal, dict) or terminal.get("status") not in (
        "completed", "failed", "cancelled", "aborted"
    ):
        raise ContractError("adapter result terminal.status is invalid")
    if not isinstance(result.get("final_text"), str):
        raise ContractError("adapter result final_text must be a string")
    for key in ("turns", "tool_calls"):
        if not isinstance(result.get(key), int) or result[key] < 0:
            raise ContractError(f"adapter result {key} must be non-negative")
    usage = result.get("usage")
    if not isinstance(usage, dict):
        raise ContractError("adapter result usage must be an object")
    for key in ("input", "output", "cache_read", "cache_write"):
        if not isinstance(usage.get(key), int) or usage[key] < 0:
            raise ContractError(f"adapter result usage.{key} must be non-negative")
    if not isinstance(result.get("trace"), list):
        raise ContractError("adapter result trace must be an array")
    if "cost" in result:
        validate_cost_report(result["cost"])
    if "provider_error" in result:
        validate_provider_error(result["provider_error"])


def validate_provider_error(provider_error: Any) -> None:
    """Validate optional provider classification without retaining remote response text."""
    if not isinstance(provider_error, dict):
        raise ContractError("adapter result provider_error must be an object")
    allowed = {"source", "status_code", "error_type", "error_code", "retryable"}
    if set(provider_error).difference(allowed):
        raise ContractError("adapter result provider_error contains an unapproved field")
    if provider_error.get("source") not in {"gateway", "adapter"}:
        raise ContractError("adapter result provider_error source is invalid")
    status_code = provider_error.get("status_code")
    if status_code is not None and (
        not isinstance(status_code, int)
        or isinstance(status_code, bool)
        or not 100 <= status_code <= 599
    ):
        raise ContractError("adapter result provider_error status_code is invalid")
    for key in ("error_type", "error_code"):
        value = provider_error.get(key)
        if value is not None and not isinstance(value, str):
            raise ContractError(f"adapter result provider_error {key} must be a string or null")
    retryable = provider_error.get("retryable")
    if retryable is not None and not isinstance(retryable, bool):
        raise ContractError("adapter result provider_error retryable must be a boolean or null")


def validate_cost_report(cost: Any) -> None:
    """Validate optional, redacted provider accounting without pricing it locally."""
    if not isinstance(cost, dict) or cost.get("schema_version") != "tea-eval-cost/v1":
        raise ContractError("adapter result cost must be a tea-eval-cost/v1 object")
    if cost.get("currency") != "USD" or cost.get("pricing") != "provider_reported":
        raise ContractError("adapter result cost must be USD and provider_reported")
    turns = cost.get("turns")
    if not isinstance(turns, list):
        raise ContractError("adapter result cost turns must be an array")
    for key in ("reported_turn_count", "unavailable_turn_count"):
        if not isinstance(cost.get(key), int) or isinstance(cost[key], bool) or cost[key] < 0:
            raise ContractError(f"adapter result cost {key} must be non-negative")
    if cost["reported_turn_count"] + cost["unavailable_turn_count"] != len(turns):
        raise ContractError("adapter result cost turn counters must match turn records")
    if cost.get("complete") != (cost["unavailable_turn_count"] == 0):
        raise ContractError("adapter result cost completeness must match unavailable turns")
    for turn in turns:
        if not isinstance(turn, dict) or turn.get("source") not in {
            "openrouter_generation", "openrouter_stream_usage", "openrouter_chat_usage", "unavailable",
        }:
            raise ContractError("adapter result cost has an invalid turn source")
    for key in ("reported_total_usd", "reported_upstream_inference_usd"):
        value = cost.get(key)
        if not isinstance(value, (int, float)) or isinstance(value, bool) or not math.isfinite(value) or value < 0:
            raise ContractError(f"adapter result cost {key} must be non-negative")


def attempt_envelope(
    task: dict[str, Any], baseline: dict[str, Any], attempt_id: str, workspace_hash: str,
    status: str, elapsed_ms: int, exit_code: int | None, timed_out: bool,
    result: dict[str, Any] | None, oracle: dict[str, Any], comparison: dict[str, Any]
) -> dict[str, Any]:
    terminal = result.get("terminal") if isinstance(result, dict) else None
    usage = result.get("usage") if isinstance(result, dict) else None
    cost = result.get("cost") if isinstance(result, dict) else None
    return {
        "schema_version": "tea-coding-eval-attempt/v1",
        "attempt_id": attempt_id,
        "baseline_id": baseline["id"],
        "task_id": task["task_id"],
        "task_version": task["task_version"],
        "task_hash": digest(task),
        "capability_hash": digest(task["capabilities"]),
        "workspace_input_hash": workspace_hash,
        "model": comparison["model"],
        "provider_revision": comparison["provider_revision"],
        "sampling": comparison["sampling"],
        "profile_version": baseline["profile_version"],
        "runtime_version": baseline["runtime_version"],
        "revision": baseline["revision"],
        "status": status,
        "elapsed_ms": elapsed_ms,
        "exit_code": exit_code,
        "timed_out": timed_out,
        "cancelled": isinstance(terminal, dict) and terminal.get("status") in ("cancelled", "aborted"),
        "terminal_status": terminal.get("status") if isinstance(terminal, dict) else None,
        "turns": result.get("turns") if isinstance(result, dict) else None,
        "tool_calls": result.get("tool_calls") if isinstance(result, dict) else None,
        "usage": usage,
        "cost": cost,
        "trace": result.get("trace", []) if isinstance(result, dict) else [],
        "oracle": oracle,
        "adapter_result": result,
    }


def run_attempt(
    task: dict[str, Any], baseline: dict[str, Any], *, attempt_id: str,
    workspace_parent: Path, allow_provider: bool, comparison: dict[str, Any]
) -> dict[str, Any]:
    if not allow_provider:
        raise ContractError("run requires --allow-provider")
    workspace = materialize_workspace(task, workspace_parent)
    started = time.monotonic()
    task_path = workspace.parent / f"{attempt_id}.task.json"
    result_path = workspace.parent / f"{attempt_id}.result.json"
    capabilities_path = workspace.parent / f"{attempt_id}.capabilities.json"
    task_path.write_bytes(canonical(task))
    capabilities_path.write_bytes(canonical(task["capabilities"]))
    command = render_command(baseline["command"], {
        "{task_json}": str(task_path),
        "{workspace}": str(workspace),
        "{result_json}": str(result_path),
        "{capabilities_json}": str(capabilities_path),
        "{attempt_id}": attempt_id,
        "{baseline_id}": baseline["id"],
        "{controller_root}": str(ROOT),
        "{controller_python}": sys.executable,
    })
    completed: subprocess.CompletedProcess[str] | None = None
    timed_out = False
    adapter_error: str | None = None
    try:
        completed, timed_out = run_adapter_process(
            command,
            cwd=workspace,
            environment=safe_environment(),
            timeout_seconds=task["timeout_seconds"],
        )
        exit_code: int | None = completed.returncode
    except OSError as exc:
        adapter_error, exit_code = str(exc), None
    adapter_result: dict[str, Any] | None = None
    if result_path.is_file():
        try:
            adapter_result = read_json(result_path)
        except ContractError:
            adapter_error = "adapter result is not valid JSON"
    if adapter_result is not None:
        try:
            validate_adapter_result(
                adapter_result, attempt_id=attempt_id, baseline_id=baseline["id"]
            )
        except ContractError as exc:
            adapter_error = str(exc)
    if adapter_error:
        oracle = {"oracle_id": task["oracle_id"], "status": "failed", "detail": adapter_error}
    elif timed_out:
        oracle = {"oracle_id": task["oracle_id"], "status": "failed", "detail": "timeout"}
    elif exit_code != 0:
        detail = "adapter exited without a result"
        if completed is not None:
            detail = (completed.stdout + "\n" + completed.stderr).strip()[-2_000:] or detail
        oracle = {"oracle_id": task["oracle_id"], "status": "failed", "detail": detail}
    else:
        oracle = verify_result(task, workspace, adapter_result or {})
    elapsed_ms = int((time.monotonic() - started) * 1_000)
    status = "success" if oracle["status"] == "passed" else "failure"
    record = attempt_envelope(
        task, baseline, attempt_id, digest(task["initial_workspace"]), status, elapsed_ms,
        exit_code, timed_out, adapter_result, oracle, comparison
    )
    if status == "success":
        shutil.rmtree(workspace, ignore_errors=True)
    for path in (task_path, capabilities_path, result_path):
        try:
            path.unlink()
        except FileNotFoundError:
            pass
    return record


def paired_plan(tasks: list[dict[str, Any]], config: dict[str, Any]) -> list[dict[str, Any]]:
    rng = random.Random(config["seed"])
    plan: list[dict[str, Any]] = []
    for repeat in range(config["repeats"]):
        for task in tasks:
            order = list(config["baselines"])
            rng.shuffle(order)
            for baseline in order:
                plan.append({
                    "repeat": repeat, "task_id": task["task_id"], "baseline_id": baseline["id"],
                    "wave": "ready" if task["kind"] == "control" else "coding",
                })
    return plan


def execute_wave(
    items: list[dict[str, Any]], tasks_by_id: dict[str, dict[str, Any]],
    baselines_by_id: dict[str, dict[str, Any]], *, wave_name: str, wave: dict[str, Any],
    workspace_parent: Path, comparison: dict[str, Any], run_attempt_fn=run_attempt,
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    """Run one baseline/wave group with bounded, observable host concurrency.

    The controller does not claim that a provider admitted every logical task.
    `concurrency` describes the requested wave and `admission_concurrency` is
    the maximum number of adapter commands this controller starts at once.
    The smaller value is the actual local worker limit. Results are restored
    to plan order even though attempts settle independently.
    """
    if not items:
        return [], {
            "wave": wave_name, "planned_attempts": 0, "attempted": 0,
            "logical_concurrency": wave["concurrency"],
            "admission_concurrency": wave["admission_concurrency"],
            "observed_active_peak": 0, "stopped": False,
        }
    baseline_ids = {item["baseline_id"] for item in items}
    if len(baseline_ids) != 1 or any(item["wave"] != wave_name for item in items):
        raise ContractError("execute_wave requires one baseline and one wave")

    worker_limit = min(wave["concurrency"], wave["admission_concurrency"])
    active, active_peak = 0, 0
    activity_lock = Lock()

    def worker(index: int, item: dict[str, Any]) -> tuple[int, dict[str, Any]]:
        nonlocal active, active_peak
        with activity_lock:
            active += 1
            active_peak = max(active_peak, active)
        try:
            task = tasks_by_id[item["task_id"]]
            baseline = baselines_by_id[item["baseline_id"]]
            attempt_id = f"{task['task_id']}-r{item['repeat']}-{baseline['id']}"
            record = run_attempt_fn(
                task, baseline, attempt_id=attempt_id, workspace_parent=workspace_parent,
                allow_provider=True, comparison=comparison,
            )
            record["wave"] = wave_name
            return index, record
        finally:
            with activity_lock:
                active -= 1

    records_by_index: dict[int, dict[str, Any]] = {}
    next_index, started, stopped = 0, 0, False
    futures: dict[Any, int] = {}
    with ThreadPoolExecutor(max_workers=worker_limit, thread_name_prefix="tea-eval") as executor:
        def start_until_full() -> None:
            nonlocal next_index, started
            while not stopped and next_index < len(items) and len(futures) < worker_limit:
                if started and wave["stagger_ms"]:
                    time.sleep(wave["stagger_ms"] / 1_000)
                item = items[next_index]
                future = executor.submit(worker, next_index, item)
                futures[future] = next_index
                next_index += 1
                started += 1

        start_until_full()
        while futures:
            done, _ = wait(futures, return_when=FIRST_COMPLETED)
            for future in done:
                index = futures.pop(future)
                returned_index, record = future.result()
                if returned_index != index:
                    raise ContractError("evaluation worker returned an unexpected plan index")
                records_by_index[index] = record
                if record["status"] == "failure" and wave["stop_on_failure"]:
                    stopped = True
            start_until_full()

    records = [records_by_index[index] for index in sorted(records_by_index)]
    return records, {
        "wave": wave_name,
        "baseline_id": next(iter(baseline_ids)),
        "planned_attempts": len(items),
        "attempted": len(records),
        "logical_concurrency": wave["concurrency"],
        "admission_concurrency": wave["admission_concurrency"],
        "observed_active_peak": active_peak,
        "stagger_ms": wave["stagger_ms"],
        "stop_on_failure": wave["stop_on_failure"],
        "stopped": stopped,
    }


def percentile(values: list[float], fraction: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    index = min(len(ordered) - 1, math.ceil(fraction * len(ordered)) - 1)
    return ordered[index]


def wilson_interval(successes: int, attempts: int) -> list[float] | None:
    if attempts == 0:
        return None
    z = 1.96
    observed = successes / attempts
    denominator = 1 + z * z / attempts
    center = (observed + z * z / (2 * attempts)) / denominator
    radius = z * math.sqrt(observed * (1 - observed) / attempts + z * z / (4 * attempts * attempts)) / denominator
    return [max(0.0, center - radius), min(1.0, center + radius)]


def summarize(records: list[dict[str, Any]]) -> dict[str, Any]:
    summary: dict[str, Any] = {}
    for baseline_id in sorted({record["baseline_id"] for record in records}):
        selected = [record for record in records if record["baseline_id"] == baseline_id]
        elapsed = [float(record["elapsed_ms"]) for record in selected]
        turns = [float(record["turns"]) for record in selected if isinstance(record.get("turns"), int)]
        calls = [float(record["tool_calls"]) for record in selected if isinstance(record.get("tool_calls"), int)]
        token_totals = [
            sum(usage.get(key, 0) for key in ("input", "output", "cache_read", "cache_write"))
            for record in selected
            if isinstance((usage := record.get("usage")), dict)
        ]
        reported_costs = [
            float(cost["reported_total_usd"])
            for record in selected
            if isinstance((cost := record.get("cost")), dict) and cost.get("complete")
        ]
        incomplete_costs = sum(
            1
            for record in selected
            if not (isinstance((cost := record.get("cost")), dict) and cost.get("complete"))
        )
        successes = sum(record["status"] == "success" for record in selected)
        summary[baseline_id] = {
            "attempts": len(selected),
            "successes": successes,
            "success_rate": successes / len(selected) if selected else None,
            "success_rate_95ci": wilson_interval(successes, len(selected)),
            "elapsed_ms_median": statistics.median(elapsed) if elapsed else None,
            "elapsed_ms_p95": percentile(elapsed, 0.95),
            "turns_median": statistics.median(turns) if turns else None,
            "tool_calls_median": statistics.median(calls) if calls else None,
            "tokens_median": statistics.median(token_totals) if token_totals else None,
            "provider_reported_cost_usd": {
                "complete_attempts": len(reported_costs),
                "incomplete_or_unreported_attempts": incomplete_costs,
                "total": sum(reported_costs) if reported_costs else None,
                "median": statistics.median(reported_costs) if reported_costs else None,
            },
        }
    return summary


def paired_cost_comparison(summary: dict[str, Any]) -> dict[str, Any]:
    """Compare aggregate provider-reported USD totals without manufacturing missing prices."""
    totals: dict[str, float | None] = {}
    complete = True
    for baseline_id in ("upstream", "rust"):
        baseline = summary.get(baseline_id)
        cost = baseline.get("provider_reported_cost_usd") if isinstance(baseline, dict) else None
        total = cost.get("total") if isinstance(cost, dict) else None
        totals[baseline_id] = total if isinstance(total, (int, float)) and not isinstance(total, bool) else None
        complete = complete and isinstance(cost, dict) and cost.get("incomplete_or_unreported_attempts") == 0 and totals[baseline_id] is not None
    upstream, rust = totals["upstream"], totals["rust"]
    return {
        "schema_version": "tea-eval-cost-comparison/v1",
        "currency": "USD",
        "complete": complete,
        "upstream_total_usd": upstream,
        "rust_total_usd": rust,
        "rust_minus_upstream_usd": None if upstream is None or rust is None else rust - upstream,
    }


def command_validate(args: argparse.Namespace) -> int:
    tasks = select_tasks(load_tasks(Path(args.tasks)), args.task)
    if args.baselines:
        load_baselines(Path(args.baselines))
    print(json.dumps({"status": "valid", "tasks": [task["task_id"] for task in tasks]}, indent=2))
    return 0


def command_plan(args: argparse.Namespace) -> int:
    tasks, config = select_tasks(load_tasks(Path(args.tasks)), args.task), load_baselines(Path(args.baselines))
    print(json.dumps({"status": "planned", "runs": paired_plan(tasks, config)}, indent=2))
    return 0


def command_run(args: argparse.Namespace) -> int:
    if not args.allow_provider:
        raise ContractError("refusing provider execution: pass --allow-provider explicitly")
    tasks, config = select_tasks(load_tasks(Path(args.tasks)), args.task), load_baselines(Path(args.baselines))
    tasks_by_id = {task["task_id"]: task for task in tasks}
    baselines_by_id = {baseline["id"]: baseline for baseline in config["baselines"]}
    grouped: dict[tuple[str, str], list[dict[str, Any]]] = {}
    for item in paired_plan(tasks, config):
        grouped.setdefault((item["baseline_id"], item["wave"]), []).append(item)

    records: list[dict[str, Any]] = []
    wave_reports: list[dict[str, Any]] = []
    for (_baseline_id, wave_name), items in grouped.items():
        wave_records, wave_report = execute_wave(
            items, tasks_by_id, baselines_by_id, wave_name=wave_name,
            wave=config["waves"][wave_name], workspace_parent=Path(args.workspace_root),
            comparison=config["comparison"],
        )
        records.extend(wave_records)
        wave_reports.append(wave_report)
    summary = summarize(records)
    report = {
        "schema_version": "tea-coding-eval-report/v1",
        "records": records,
        "summary": summary,
        "provider_reported_cost_comparison": paired_cost_comparison(summary),
        "waves": wave_reports,
    }
    if args.out:
        output = Path(args.out)
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    else:
        print(json.dumps(report, indent=2))
    complete = all(not wave["stopped"] for wave in wave_reports)
    return 0 if complete and records and all(record["status"] == "success" for record in records) else 1


def make_parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    sub = command.add_subparsers(dest="command", required=True)
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--tasks", default=str(TASKS))
    common.add_argument("--task", action="append", default=[], help="run one named task contract (repeatable)")
    validate = sub.add_parser("validate", parents=[common])
    validate.add_argument("--baselines")
    validate.set_defaults(handler=command_validate)
    plan = sub.add_parser("plan", parents=[common])
    plan.add_argument("--baselines", required=True)
    plan.set_defaults(handler=command_plan)
    run = sub.add_parser("run", parents=[common])
    run.add_argument("--baselines", required=True)
    run.add_argument("--allow-provider", action="store_true")
    run.add_argument("--workspace-root", default=str(ROOT / ".workspaces"))
    run.add_argument("--out")
    run.set_defaults(handler=command_run)
    return command


def main(argv: list[str] | None = None) -> int:
    args = make_parser().parse_args(argv)
    try:
        return args.handler(args)
    except ContractError as exc:
        print(f"eval contract error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
