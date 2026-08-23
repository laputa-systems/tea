"""Repository-owned isolated runner for the disabled-multiedit evaluation.

The candidate is an explicit process adapter, not a trusted report producer.
For every phase the runner creates a fresh workspace, invokes the command with
only public task material plus the workspace authority, snapshots filesystem
state itself, and derives the grade record. Candidates write only the narrow
``runner-result.json`` protocol receipt described in ``README.md``.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time
from typing import Any, Mapping, Sequence

from .multiedit import (
    DESIGN_RUBRIC,
    EFFICIENCY_METRICS,
    PUBLIC_CAPABILITIES,
    SCHEMA,
    TRUSTED_RUNNER_ID,
    MultiEditQualityError,
    hidden_case,
    hidden_case_digest,
    write_multiedit_task,
)


RESULT_FILE = "runner-result.json"
OUTPUT_LIMIT_BYTES = 64 * 1024
PROBE_NAMES = (
    "stale",
    "overlap_duplicate",
    "workspace_escape",
    "non_regular",
    "fault",
    "cancel_before_commit",
    "cancel_after_commit",
)


class MultiEditRunError(MultiEditQualityError):
    """The isolated candidate process did not satisfy the runner protocol."""


def run_multiedit(
    *, out: Path, command: Sequence[str], timeout_seconds: float = 30.0
) -> dict[str, Any]:
    """Execute one candidate command and produce a runner-owned grade record.

    The command runs once for the ordinary hidden workspace and once for each
    named probe. It receives task/workspace/phase through the environment;
    its only response channel is a bounded JSON receipt in its own directory.
    The candidate never receives the hidden-case file or a grader-record path.
    """

    if not command:
        raise MultiEditRunError("multiedit runner requires a candidate command after --command")
    if not timeout_seconds > 0:
        raise MultiEditRunError("multiedit runner timeout must be positive")
    task = write_multiedit_task(out)
    runner_root = out / "runner-workspace"
    if runner_root.exists():
        raise MultiEditRunError(f"runner workspace already exists: {runner_root}")
    runner_root.mkdir(parents=True)
    candidate_root = runner_root / "candidate"
    candidate_root.mkdir()
    # The candidate can inspect its public task copy but never the checked-in
    # hidden case. The actual operation workspaces are separate siblings.
    public_task = candidate_root / "task.json"
    shutil.copyfile(task, public_task)
    case = hidden_case()
    started = time.monotonic()
    normal_workspace = runner_root / "workspace"
    _materialize_initial_workspace(case, normal_workspace)
    normal = _invoke_candidate(
        command=command,
        candidate_root=candidate_root,
        task=public_task,
        workspace=normal_workspace,
        phase="normal",
        timeout_seconds=timeout_seconds,
    )
    probes: dict[str, dict[str, object]] = {}
    for name in PROBE_NAMES:
        workspace = runner_root / "probes" / name
        _materialize_initial_workspace(case, workspace)
        _prepare_probe(name, workspace)
        before = _snapshot_workspace(workspace)
        receipt = _invoke_candidate(
            command=command,
            candidate_root=candidate_root,
            task=public_task,
            workspace=workspace,
            phase=name,
            timeout_seconds=timeout_seconds,
        )
        unchanged = before == _snapshot_workspace(workspace)
        probes[name] = _derive_probe(name, receipt, unchanged, workspace)
    elapsed_ms = round((time.monotonic() - started) * 1000)
    design = _score_design(candidate_root / "design.md")
    efficiency = _runner_efficiency(
        task=public_task,
        elapsed_ms=elapsed_ms,
        phase_count=1 + len(PROBE_NAMES),
        output_bytes=normal["output_bytes"],
    )
    record: dict[str, Any] = {
        "schema_version": SCHEMA,
        "capabilities": list(PUBLIC_CAPABILITIES),
        "trusted_grader": {
            "runner_id": TRUSTED_RUNNER_ID,
            "case_id": case["id"],
            "case_digest": hidden_case_digest(case),
        },
        "validation": {"workspace": str(normal_workspace), "probes": probes},
        "design_rubric": design,
        "efficiency": efficiency,
        "runner_observation": {
            "candidate_command": list(command),
            "normal_exit_code": normal["exit_code"],
            "phase_count": 1 + len(PROBE_NAMES),
            "workspace_root": str(runner_root),
        },
    }
    record_path = out / "record.json"
    record_path.write_text(json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return record


def _materialize_initial_workspace(case: Mapping[str, Any], workspace: Path) -> None:
    initial = case.get("initial_files")
    if not isinstance(initial, Mapping) or not initial:
        raise MultiEditRunError("hidden multiedit case requires runner-owned initial_files")
    workspace.mkdir(parents=True)
    for relative, content in initial.items():
        if not isinstance(relative, str) or not isinstance(content, str):
            raise MultiEditRunError("hidden multiedit initial files must be string paths and UTF-8 text")
        path = _safe_workspace_path(workspace, relative)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")


def _prepare_probe(name: str, workspace: Path) -> None:
    if name == "stale":
        (workspace / "lib" / "alpha.txt").write_text("concurrent stale change\n", encoding="utf-8")
    elif name == "non_regular":
        target = workspace / "lib" / "not-a-file"
        target.mkdir()
    elif name == "workspace_escape":
        # The process receives this untrusted lexical path only through its
        # phase name; a safe candidate must refuse rather than normalize it.
        (workspace / "outside-sentinel.txt").write_text("sentinel\n", encoding="utf-8")


def _invoke_candidate(
    *,
    command: Sequence[str],
    candidate_root: Path,
    task: Path,
    workspace: Path,
    phase: str,
    timeout_seconds: float,
) -> dict[str, object]:
    result_path = candidate_root / RESULT_FILE
    result_path.unlink(missing_ok=True)
    environment = {
        "PATH": os.environ.get("PATH", ""),
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
        "HOME": str(candidate_root),
        "TEA_MULTIEDIT_TASK": str(task),
        "TEA_MULTIEDIT_WORKSPACE": str(workspace),
        "TEA_MULTIEDIT_PHASE": phase,
        "TEA_MULTIEDIT_RESULT": str(result_path),
    }
    process = subprocess.Popen(
            list(command),
            cwd=candidate_root,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
    try:
        stdout, stderr = process.communicate(timeout=timeout_seconds)
    except subprocess.TimeoutExpired as error:
        _terminate_process_group(process)
        process.communicate()
        raise MultiEditRunError(f"candidate timed out during {phase}") from error
    output_bytes = len(stdout) + len(stderr)
    if output_bytes > OUTPUT_LIMIT_BYTES:
        raise MultiEditRunError(f"candidate exceeded {OUTPUT_LIMIT_BYTES} output bytes during {phase}")
    if process.returncode != 0:
        raise MultiEditRunError(f"candidate exited {process.returncode} during {phase}")
    try:
        value = json.loads(result_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise MultiEditRunError(f"candidate did not write valid {RESULT_FILE} during {phase}: {error}") from error
    if not isinstance(value, dict) or value.get("phase") != phase:
        raise MultiEditRunError(f"candidate {RESULT_FILE} must identify phase {phase}")
    value["exit_code"] = process.returncode
    value["output_bytes"] = output_bytes
    return value


def _terminate_process_group(process: subprocess.Popen[bytes]) -> None:
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except (AttributeError, OSError):
        process.kill()


def _derive_probe(
    name: str, receipt: Mapping[str, object], unchanged: bool, workspace: Path
) -> dict[str, object]:
    outcome = receipt.get("outcome")
    if name in ("stale", "overlap_duplicate", "workspace_escape", "non_regular"):
        return {"outcome": outcome, "unchanged": unchanged}
    if name == "fault":
        inspected = receipt.get("inspected_paths")
        expected_paths = {"lib/alpha.txt", "lib/beta.txt"}
        return {
            "outcome": outcome,
            "all_targets_inspected": isinstance(inspected, list) and set(inspected) == expected_paths,
            "unchanged": unchanged,
        }
    if name == "cancel_before_commit":
        return {"outcome": outcome, "unchanged": unchanged}
    if name == "cancel_after_commit":
        return {
            "receipt": receipt.get("receipt"),
            # A successful process exit after the response file is observed is
            # the runner-owned settlement boundary for this protocol.
            "settled_before_agent_end": True,
            "workspace_snapshot": _workspace_digest(workspace),
        }
    raise AssertionError(f"unknown probe {name}")


def _score_design(path: Path) -> dict[str, int]:
    try:
        text = path.read_text(encoding="utf-8").lower()
    except OSError:
        text = ""
    return {
        "contract": 5 if all(word in text for word in ("stale", "atomic", "cancellation")) else 0,
        "proof": 5 if all(word in text for word in ("proof", "test", "receipt")) else 0,
        "limitations": 5 if all(word in text for word in ("limitation", "recovery")) else 0,
    }


def _runner_efficiency(*, task: Path, elapsed_ms: int, phase_count: int, output_bytes: object) -> dict[str, int]:
    if not isinstance(output_bytes, int):
        output_bytes = 0
    values = {
        "tool_calls": phase_count,
        "turns": phase_count,
        "wall_clock_ms": elapsed_ms,
        "output_tokens": (output_bytes + 3) // 4,
        "remote_round_trips": 0,
        "context_bytes": task.stat().st_size,
    }
    return {name: values[name] for name in EFFICIENCY_METRICS}


def _safe_workspace_path(workspace: Path, relative: str) -> Path:
    candidate = workspace.joinpath(*Path(relative).parts)
    if Path(relative).is_absolute() or ".." in Path(relative).parts or candidate.parent == workspace.parent:
        raise MultiEditRunError("hidden multiedit path escapes the runner workspace")
    return candidate


def _snapshot_workspace(workspace: Path) -> dict[str, tuple[str, str]]:
    snapshot: dict[str, tuple[str, str]] = {}
    for path in sorted(workspace.rglob("*")):
        relative = str(path.relative_to(workspace))
        info = path.lstat()
        if path.is_dir():
            snapshot[relative] = ("directory", "")
        elif path.is_file():
            snapshot[relative] = ("file", hashlib.sha256(path.read_bytes()).hexdigest())
        else:
            snapshot[relative] = ("other", str(info.st_mode))
    return snapshot


def _workspace_digest(workspace: Path) -> str:
    encoded = json.dumps(_snapshot_workspace(workspace), separators=(",", ":"), sort_keys=True).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()
