"""Provider-opt-in ecological coding evaluations for the three pinned Express cases.

This module owns clean worktrees, validation, artifacts, and process resource
measurements around the Rust coding adapter. It never discovers a model or a
credential. Callers explicitly select an env file; a tiny shell boundary
sources it immediately before the adapter starts, so Python and tool children
never receive or persist the OpenRouter credential.
"""

from __future__ import annotations

import hashlib
import json
import math
import os
from pathlib import Path
import signal
import subprocess
import sys
from typing import Any, Iterable

from .coding_cases import (
    CodingCaseError,
    dependency_cache_path,
    load_cases,
    materialize_clean_worktree,
    remove_worktree,
    run_validator,
)


ROOT = Path(__file__).resolve().parents[2]
CODING_BUILTINS_ROOT = ROOT / "crates" / "tea-luau" / "builtins"
CODING_BUILTIN_NAMES = ("read", "bash", "edit", "find")
RESULT_SCHEMA = "tea-coding-eval-result/v1"
CODING_SCHEMA = "tea-quality-coding-run/v1"


class CodingRunError(RuntimeError):
    """A live coding-evaluation process or artifact violated its contract."""


def _canonical(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")


def _digest(value: Any) -> str:
    return hashlib.sha256(_canonical(value)).hexdigest()


def _safe_environment() -> dict[str, str]:
    return {
        key: value
        for key, value in {"PATH": os.environ.get("PATH", ""), "LANG": "C", "LC_ALL": "C"}.items()
        if value
    }


def _time_command(command: list[str]) -> tuple[list[str], str | None]:
    time = Path("/usr/bin/time")
    if not time.is_file():
        return command, None
    if sys.platform == "darwin":
        return [str(time), "-l", *command], "darwin"
    return [str(time), "-v", *command], "gnu"


def _peak_rss(stderr: str, style: str | None) -> int | None:
    if style is None:
        return None
    for line in stderr.splitlines():
        text = line.strip().lower()
        if style == "darwin" and text.endswith("maximum resident set size"):
            try:
                return int(text.split()[0])
            except (IndexError, ValueError):
                return None
        if style == "gnu" and "maximum resident set size" in text:
            try:
                return int(text.rsplit(":", 1)[1].strip()) * 1024
            except (IndexError, ValueError):
                return None
    return None


def coding_bundle_capabilities() -> list[dict[str, Any]]:
    # The coding surface is a closed set of independent Luau builtins. Verify
    # each checked-in manifest at the process boundary instead of relying on
    # the pre-split aggregate ``builtins/coding/manifest.json`` that no longer
    # exists. The concrete provider surface is still resolved by the Rust
    # adapter; this task manifest records only the shared four-tool contract.
    for name in CODING_BUILTIN_NAMES:
        manifest_path = CODING_BUILTINS_ROOT / name / "manifest.json"
        try:
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise CodingRunError(f"cannot read default {name} builtin manifest: {error}") from error
        if not isinstance(manifest, dict) or manifest.get("id") != name:
            raise CodingRunError(f"default {name} builtin manifest is invalid")
    capabilities = [
        {"name": name, "kind": "tea_coding_bundle", "description": None, "schema": {"type": "object"}}
        for name in CODING_BUILTIN_NAMES
    ]
    return capabilities


def _adapter_task(case: dict[str, Any], capabilities: list[dict[str, Any]]) -> dict[str, Any]:
    task = case["task"]
    return {
        "schema_version": "tea-coding-eval-task/v1",
        "task_id": case["id"],
        "task_version": 1,
        "kind": "coding",
        "prompt": task["prompt"],
        "initial_workspace": [],
        "capabilities": capabilities,
        "timeout_seconds": 180,
        "oracle_id": "quality-express-validator-v1",
    }


def _result_contract(result: Any, *, attempt_id: str, baseline_id: str) -> dict[str, Any]:
    if not isinstance(result, dict):
        raise CodingRunError("adapter result must be a JSON object")
    if result.get("schema_version") != RESULT_SCHEMA:
        raise CodingRunError("adapter result has the wrong schema_version")
    if result.get("attempt_id") != attempt_id or result.get("baseline_id") != baseline_id:
        raise CodingRunError("adapter result identity does not match its explicit invocation")
    terminal = result.get("terminal")
    if not isinstance(terminal, dict) or terminal.get("status") not in {"completed", "failed", "cancelled", "aborted"}:
        raise CodingRunError("adapter result has no valid terminal status")
    _cost_contract(result.get("cost"))
    return result


def _nonnegative_number(value: Any) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value) and value >= 0


def _cost_contract(cost: Any) -> None:
    """Validate the common redacted, provider-reported accounting contract."""
    if not isinstance(cost, dict):
        raise CodingRunError("adapter result has no cost report")
    if cost.get("schema_version") != "tea-eval-cost/v1":
        raise CodingRunError("adapter cost report has the wrong schema_version")
    if cost.get("currency") != "USD" or cost.get("pricing") != "provider_reported":
        raise CodingRunError("adapter cost report must be USD and provider_reported")
    turns = cost.get("turns")
    if not isinstance(turns, list):
        raise CodingRunError("adapter cost report has no per-turn records")
    reported = cost.get("reported_turn_count")
    unavailable = cost.get("unavailable_turn_count")
    if not isinstance(reported, int) or isinstance(reported, bool) or reported < 0:
        raise CodingRunError("adapter cost report has an invalid reported_turn_count")
    if not isinstance(unavailable, int) or isinstance(unavailable, bool) or unavailable < 0:
        raise CodingRunError("adapter cost report has an invalid unavailable_turn_count")
    if reported + unavailable != len(turns):
        raise CodingRunError("adapter cost report counters do not match its turn records")
    if cost.get("complete") != (unavailable == 0):
        raise CodingRunError("adapter cost completeness does not match unavailable turns")
    for total_name in ("reported_total_usd", "reported_upstream_inference_usd"):
        if not _nonnegative_number(cost.get(total_name)):
            raise CodingRunError(f"adapter cost report has invalid {total_name}")
    for turn in turns:
        if not isinstance(turn, dict) or turn.get("source") not in {
            "openrouter_generation",
            "openrouter_stream_usage",
            "openrouter_chat_usage",
            "unavailable",
        }:
            raise CodingRunError("adapter cost report has an invalid per-turn source")
        total = turn.get("total_usd")
        if total is not None and not _nonnegative_number(total):
            raise CodingRunError("adapter cost report has an invalid per-turn total")
        if turn.get("source") == "unavailable" and total is not None:
            raise CodingRunError("unavailable adapter cost turn must not have a total")


def _cost_comparison(adapter_records: list[dict[str, Any]]) -> dict[str, Any]:
    by_adapter = {
        record["adapter"]: record["adapter_result"].get("cost")
        for record in adapter_records
        if isinstance(record.get("adapter_result"), dict)
    }
    rust = by_adapter.get("rust")
    rust_total = rust.get("reported_total_usd") if isinstance(rust, dict) and rust.get("complete") else None
    return {
        "schema_version": "tea-eval-cost-comparison/v1",
        "currency": "USD",
        "complete": rust_total is not None,
        "rust_total_usd": rust_total,
    }


def _run_process(command: list[str], *, cwd: Path, timeout_seconds: int) -> tuple[int | None, bool, str, str, int | None]:
    measured, style = _time_command(command)
    process = subprocess.Popen(
        measured,
        cwd=cwd,
        env=_safe_environment(),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=os.name == "posix",
    )
    try:
        stdout, stderr = process.communicate(timeout=timeout_seconds)
        return process.returncode, False, stdout, stderr, _peak_rss(stderr, style)
    except subprocess.TimeoutExpired:
        if os.name == "posix":
            os.killpg(process.pid, signal.SIGTERM)
        else:
            process.terminate()
        try:
            stdout, stderr = process.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            if os.name == "posix":
                os.killpg(process.pid, signal.SIGKILL)
            else:
                process.kill()
            stdout, stderr = process.communicate()
        return None, True, stdout, stderr, _peak_rss(stderr, style)


def _adapter_command(adapter: str, model: str, task_path: Path, workspace: Path, capabilities_path: Path, result_path: Path, attempt_id: str) -> list[str]:
    if adapter != "rust":
        raise CodingRunError(f"unsupported coding adapter {adapter!r}")
    script = ROOT / "evals" / "run-rust-live.sh"
    return [
        "bash",
        str(script),
        "--model",
        model,
        "--task-json",
        str(task_path),
        "--workspace",
        str(workspace),
        "--capabilities-json",
        str(capabilities_path),
        "--result-json",
        str(result_path),
        "--attempt-id",
        attempt_id,
        "--baseline-id",
        adapter,
    ]


def _env_sourced_command(env_file: Path, command: list[str]) -> list[str]:
    """Source a caller-designated env file in the final process boundary only."""
    return [
        "bash",
        "-c",
        'set -a; . "$1"; set +a; shift; exec "$@"',
        "tea-quality-source-env",
        str(env_file),
        *command,
    ]


def _resolve_env_file(env_file: Path) -> Path:
    try:
        resolved = env_file.resolve(strict=True)
    except OSError as error:
        raise CodingRunError(f"cannot resolve explicit provider env file: {error}") from error
    if not resolved.is_file():
        raise CodingRunError("explicit provider env path must be a regular file")
    return resolved


def prepare_cache(*, cache_root: Path, case_ids: Iterable[str] | None = None) -> dict[str, Any]:
    """Populate exact bare repositories before any offline scoring attempt."""
    selected = set(case_ids or ())
    cases = [case for case in load_cases() if not selected or case["id"] in selected]
    missing = selected - {case["id"] for case in cases}
    if missing:
        raise CodingRunError(f"unknown coding case(s): {', '.join(sorted(missing))}")
    cached: list[str] = []
    for case in cases:
        # Materialization owns the cache protocol. This private import avoids
        # duplicating its repository/commit allowlist at the CLI boundary.
        from .coding_cases import cache_bare_repository

        cache_bare_repository(case["baseline"]["repository"], case["baseline"]["commit"], cache_root, populate=True)
        cache_bare_repository(case["baseline"]["repository"], case["baseline"]["fix_commit"], cache_root, populate=True)
        cached.append(case["id"])
    return {"schema_version": CODING_SCHEMA, "operation": "prepare-cache", "cases": cached, "cache_root": str(cache_root)}


def run_coding_cases(
    *,
    model: str,
    cache_root: Path,
    workspace_root: Path,
    out: Path,
    validator: str,
    env_file: Path,
    case_ids: Iterable[str] | None = None,
) -> tuple[int, dict[str, Any]]:
    if validator not in {"fast", "full"}:
        raise CodingRunError("validator must be fast or full")
    if not model:
        raise CodingRunError("a model must be explicitly supplied")
    env_file = _resolve_env_file(env_file)
    selected = set(case_ids or ())
    cases = [case for case in load_cases() if not selected or case["id"] in selected]
    missing = selected - {case["id"] for case in cases}
    if missing:
        raise CodingRunError(f"unknown coding case(s): {', '.join(sorted(missing))}")
    capabilities = coding_bundle_capabilities()
    destination = out.resolve()
    destination.mkdir(parents=True, exist_ok=True)
    records: list[dict[str, Any]] = []
    for case in cases:
        case_destination = destination / case["id"]
        case_destination.mkdir(parents=True, exist_ok=True)
        task = _adapter_task(case, capabilities)
        task_path = case_destination / "adapter-task.json"
        capabilities_path = case_destination / "capabilities.json"
        task_path.write_bytes(_canonical(task) + b"\n")
        capabilities_path.write_bytes(_canonical(capabilities) + b"\n")
        adapter_records: list[dict[str, Any]] = []
        for adapter in ("rust",):
            worktree = materialize_clean_worktree(case, cache_root, workspace_root)
            try:
                # Fast regression validators execute the checked-out source directly and do
                # not need npm. Some historical Express pins predate package-lock.json, so
                # require a lockfile-keyed npm cache only for the full install/test audit.
                npm_cache = dependency_cache_path(worktree.path, cache_root) if validator == "full" else None
                result_path = case_destination / f"{adapter}-result.json"
                attempt_id = f"quality-{case['id']}-{adapter}"
                adapter_command = _adapter_command(adapter, model, task_path, worktree.path, capabilities_path, result_path, attempt_id)
                command = _env_sourced_command(env_file, adapter_command)
                code, timed_out, stdout, stderr, peak_rss = _run_process(command, cwd=ROOT, timeout_seconds=180)
                result: dict[str, Any] | None = None
                contract_error: str | None = None
                if not timed_out and code == 0:
                    try:
                        result = _result_contract(json.loads(result_path.read_text(encoding="utf-8")), attempt_id=attempt_id, baseline_id=adapter)
                    except (OSError, json.JSONDecodeError, CodingRunError) as error:
                        contract_error = str(error)
                validator_result = run_validator(case, worktree.path, validator, dependency_cache=npm_cache)
                diff = subprocess.run(
                    ["git", "diff", "--binary", "--no-ext-diff"], cwd=worktree.path, text=True, capture_output=True, check=False
                )
                patch = diff.stdout
                (case_destination / f"{adapter}.patch").write_text(patch, encoding="utf-8")
                adapter_record = {
                    "adapter": adapter,
                    "attempt_id": attempt_id,
                    "command": ["bash", "source explicit env file (redacted)", "--", *adapter_command[:2], "…"],
                    "process": {
                        "exit_code": code,
                        "timed_out": timed_out,
                        "peak_rss_bytes": peak_rss,
                        "peak_rss_source": "process_time" if peak_rss is not None else "unavailable",
                    },
                    "adapter_result": result,
                    "adapter_contract_error": contract_error,
                    "validator": {
                        "name": validator_result.name,
                        "passed": validator_result.passed,
                        "returncode": validator_result.returncode,
                        "timed_out": validator_result.timed_out,
                        "stdout": validator_result.stdout,
                        "stderr": validator_result.stderr,
                    },
                    "patch_sha256": hashlib.sha256(patch.encode("utf-8")).hexdigest(),
                    "passed": code == 0 and not timed_out and contract_error is None and validator_result.passed,
                }
                adapter_records.append(adapter_record)
                (case_destination / f"{adapter}-record.json").write_text(
                    json.dumps(adapter_record, indent=2, sort_keys=True) + "\n", encoding="utf-8"
                )
            finally:
                remove_worktree(worktree, workspace_root)
        record = {
            "id": case["id"],
            "source": case["source"],
            "baseline": case["baseline"],
            "validator": validator,
            "adapters": adapter_records,
            "cost_comparison": _cost_comparison(adapter_records),
            "passed": all(item["passed"] for item in adapter_records),
        }
        records.append(record)
        (case_destination / "record.json").write_text(json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    summary = {
        "schema_version": CODING_SCHEMA,
        "tier": "coding",
        "model": model,
        "validator": validator,
        "case_count": len(records),
        "passed": sum(record["passed"] for record in records),
        "failed_cases": [record["id"] for record in records if not record["passed"]],
        "cases": records,
    }
    (destination / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return (0 if not summary["failed_cases"] else 1), summary
