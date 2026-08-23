"""Hermetic disabled-tool evaluation for coordinated edit-tool design.

The public task receives no Tea v2 schema, host boundary, or oracle. A trusted
runner keeps the hidden case outside the disposable agent workspace, executes
the candidate, probes failure paths, and asks this module to derive checks from
the resulting workspace. The CLI must never treat agent-authored evidence as a
grader record.
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import stat
from typing import Any, Mapping


SCHEMA = "tea-multiedit-disabled-quality/v2"
PUBLIC_CAPABILITIES = ("read", "bash", "edit", "write")
TRUSTED_RUNNER_ID = "tea-quality-isolated-runner/v1"
HIDDEN_CASE_PATH = Path(__file__).resolve().parent / "cases" / "multiedit" / "hidden.json"
SEMANTIC_CHECKS = (
    "all_requested_changes_correct",
    "stale_snapshot_rejected_without_partial_write",
    "overlap_and_duplicate_target_rejected",
    "workspace_escape_rejected",
    "ordinary_file_guard",
    "fault_outcome_is_reconciled",
    "cancel_before_commit_preserves_files",
    "cancel_after_commit_settles_receipt",
)
EFFICIENCY_METRICS = (
    "tool_calls",
    "turns",
    "wall_clock_ms",
    "output_tokens",
    "remote_round_trips",
    "context_bytes",
)
DESIGN_RUBRIC = ("contract", "proof", "limitations")


class MultiEditQualityError(ValueError):
    """The provider-free multiedit evaluation input is malformed."""


def public_task() -> dict[str, Any]:
    """Return the only material copied into a disposable agent workspace."""

    return {
        "schema_version": SCHEMA,
        "id": "design-efficient-coordinated-edits",
        "capabilities": list(PUBLIC_CAPABILITIES),
        "disabled_capabilities": ["multiedit"],
        "network": False,
        "task": (
            "Design, implement, and prove an efficient tool or protocol for one coordinated "
            "change across several existing text files using only the listed capabilities. "
            "Explain stale-read, partial-change, cancellation, and recovery behavior, and include "
            "executable evidence and an honest limitations section. Do not assume an unavailable "
            "batch-edit capability exists."
        ),
        "submission": {
            "required": ["design.md", "evidence.json"],
            "note": "A separate evaluator checks the workspace, adversarial probes, and capability trace.",
        },
    }


def write_multiedit_task(out: Path) -> Path:
    """Create a public-only task directory without copying hidden grader data."""

    task_dir = out / "task"
    task_dir.mkdir(parents=True, exist_ok=True)
    unexpected = [path.name for path in task_dir.iterdir() if path.name != "task.json"]
    if unexpected:
        raise MultiEditQualityError(
            "public task directory contains unexpected material: " + ", ".join(sorted(unexpected))
        )
    task_path = task_dir / "task.json"
    task_path.write_text(json.dumps(public_task(), indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return task_path


def hidden_case() -> dict[str, Any]:
    """Load runner-owned inputs; callers must not materialize them for agents."""

    try:
        value = json.loads(HIDDEN_CASE_PATH.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise MultiEditQualityError(f"cannot load hidden multiedit case: {error}") from error
    if not isinstance(value, dict) or value.get("schema_version") != SCHEMA:
        raise MultiEditQualityError("hidden multiedit case has an unsupported schema")
    expected = value.get("expected_files")
    if not isinstance(expected, dict) or not expected:
        raise MultiEditQualityError("hidden multiedit case requires expected_files")
    for relative, digest in expected.items():
        path = Path(relative) if isinstance(relative, str) else Path("/")
        if (
            not isinstance(relative, str)
            or not relative
            or path.is_absolute()
            or ".." in path.parts
            or not isinstance(digest, str)
            or len(digest) != 64
            or any(character not in "0123456789abcdef" for character in digest)
        ):
            raise MultiEditQualityError("hidden multiedit case contains an unsafe path or digest")
    return value


def hidden_case_digest(case: Mapping[str, Any] | None = None) -> str:
    """Return the canonical digest binding a trusted record to hidden inputs."""

    encoded = json.dumps(case or hidden_case(), separators=(",", ":"), sort_keys=True).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def derive_hidden_checks(
    case: Mapping[str, Any], workspace: Path, probes: Mapping[str, Any]
) -> dict[str, bool]:
    """Derive semantic checks from a hidden workspace and runner probes.

    File digests are recomputed here; the record never carries semantic
    booleans. Probe fields are observations produced by the isolated runner
    after its own adversarial operations, not candidate `evidence.json`.
    """

    expected = case.get("expected_files")
    if not isinstance(expected, Mapping):
        raise MultiEditQualityError("hidden case requires expected_files")
    files_match = workspace.is_dir() and all(
        _digest_regular_workspace_file(workspace, path) == digest
        for path, digest in expected.items()
    )
    fault = probes.get("fault")
    cancel_before = probes.get("cancel_before_commit")
    cancel_after = probes.get("cancel_after_commit")
    return {
        "all_requested_changes_correct": files_match,
        "stale_snapshot_rejected_without_partial_write": _probe_no_partial(probes.get("stale")),
        "overlap_and_duplicate_target_rejected": _probe_no_partial(probes.get("overlap_duplicate")),
        "workspace_escape_rejected": _probe_no_partial(probes.get("workspace_escape")),
        "ordinary_file_guard": _probe_no_partial(probes.get("non_regular")),
        "fault_outcome_is_reconciled": isinstance(fault, Mapping)
        and fault.get("outcome") in ("rolled_back", "indeterminate")
        and fault.get("all_targets_inspected") is True,
        "cancel_before_commit_preserves_files": _probe_unchanged(cancel_before)
        and isinstance(cancel_before, Mapping)
        and cancel_before.get("outcome") == "cancelled",
        "cancel_after_commit_settles_receipt": isinstance(cancel_after, Mapping)
        and cancel_after.get("receipt") in ("committed", "rolled_back", "indeterminate")
        and cancel_after.get("settled_before_agent_end") is True,
    }


def grade_verified_record(record: Mapping[str, Any]) -> dict[str, Any]:
    """Grade a trusted runner record with a 70/15/15 rubric.

    `trusted_grader` explicitly binds the record to the repository-owned
    isolated runner and exact hidden case digest. The fields prevent accidental
    case mixing; they are not a signature. Trust comes from keeping record
    creation outside the candidate workspace. This is not a format for
    agent-authored output.
    """

    if not isinstance(record, Mapping):
        raise MultiEditQualityError("trusted runner record must be a mapping")
    if record.get("schema_version") != SCHEMA:
        raise MultiEditQualityError("unsupported or missing schema_version")
    if tuple(record.get("capabilities", ())) != PUBLIC_CAPABILITIES:
        raise MultiEditQualityError("record did not use the exact disabled-tool capability envelope")
    trusted = record.get("trusted_grader")
    validation = record.get("validation")
    if not isinstance(trusted, Mapping) or not isinstance(validation, Mapping):
        raise MultiEditQualityError("record requires trusted_grader and validation mappings")
    case = hidden_case()
    if (
        trusted.get("runner_id") != TRUSTED_RUNNER_ID
        or trusted.get("case_id") != case.get("id")
        or trusted.get("case_digest") != hidden_case_digest(case)
    ):
        raise MultiEditQualityError("record is not bound to the trusted hidden grader input")
    workspace = validation.get("workspace")
    probes = validation.get("probes")
    if not isinstance(workspace, str) or not isinstance(probes, Mapping):
        raise MultiEditQualityError("validation requires a workspace path and runner probe observations")
    semantic = derive_hidden_checks(case, Path(workspace), probes)
    failed = [name for name in SEMANTIC_CHECKS if not semantic[name]]
    metrics = _efficiency_metrics(record.get("efficiency"))
    design_components = _design_components(record.get("design_rubric"))
    design_score = sum(design_components.values())
    design_failed = (
        design_components["contract"] < 3
        or design_components["proof"] < 3
        or design_components["limitations"] < 2
    )
    components = {
        "tool_calls": _lower_is_better(metrics["tool_calls"], good=8, poor=32),
        "turns": _lower_is_better(metrics["turns"], good=2, poor=8),
        "wall_clock_ms": _lower_is_better(metrics["wall_clock_ms"], good=3_000, poor=30_000),
        "output_tokens": _lower_is_better(metrics["output_tokens"], good=1_500, poor=12_000),
        "remote_round_trips": _lower_is_better(metrics["remote_round_trips"], good=3, poor=18),
        "context_bytes": _lower_is_better(metrics["context_bytes"], good=64_000, poor=512_000),
    }
    efficiency_score = round(sum(components.values()) * 15 / 60)
    failed_gates = list(failed)
    if design_failed:
        failed_gates.append("design_proof_threshold")
    semantic_passed = not failed
    return {
        "schema_version": SCHEMA,
        "passed": semantic_passed and not design_failed,
        "semantic_score": 70 if semantic_passed else 0,
        "design_proof_score": design_score if semantic_passed else 0,
        "efficiency_score": efficiency_score if semantic_passed else 0,
        "total_score": 70 + design_score + efficiency_score if semantic_passed else 0,
        "failed_checks": failed_gates,
        "semantic_checks": semantic,
        "design_components": design_components,
        "metrics": metrics,
        "efficiency_components": components,
        "rubric": {"correctness": 70, "design_proof": 15, "efficiency": 15},
    }


def _probe_no_partial(value: object) -> bool:
    return isinstance(value, Mapping) and value.get("outcome") == "rejected" and value.get("unchanged") is True


def _probe_unchanged(value: object) -> bool:
    return isinstance(value, Mapping) and value.get("unchanged") is True


def _digest_regular_workspace_file(workspace: Path, relative: str) -> str | None:
    path = workspace / relative
    try:
        if not stat.S_ISREG(path.lstat().st_mode):
            return None
        return hashlib.sha256(path.read_bytes()).hexdigest()
    except OSError:
        return None


def _design_components(value: object) -> dict[str, int]:
    if not isinstance(value, Mapping):
        raise MultiEditQualityError("design_rubric must be a trusted grader mapping")
    result: dict[str, int] = {}
    for name in DESIGN_RUBRIC:
        score = value.get(name)
        if not isinstance(score, int) or not 0 <= score <= 5:
            raise MultiEditQualityError(f"design_rubric.{name} must be an integer from 0 through 5")
        result[name] = score
    return result


def _efficiency_metrics(value: object) -> dict[str, int]:
    if not isinstance(value, Mapping):
        raise MultiEditQualityError("efficiency must be a mapping")
    metrics: dict[str, int] = {}
    for name in EFFICIENCY_METRICS:
        item = value.get(name)
        if not isinstance(item, int) or item < 0:
            raise MultiEditQualityError(f"efficiency.{name} must be a non-negative integer")
        metrics[name] = item
    return metrics


def _lower_is_better(value: int, *, good: int, poor: int) -> int:
    if value <= good:
        return 10
    if value >= poor:
        return 0
    return round(10 * (poor - value) / (poor - good))
