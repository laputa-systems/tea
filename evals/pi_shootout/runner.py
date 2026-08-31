"""Sequential, evidence-first runner for the pinned Pi/Tea shootout.

The module owns attempt placement and process/secret boundaries.  It reuses the
quality case cache, isolated worktree, and validator rather than creating a
second benchmark substrate.
"""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor, TimeoutError as FuturesTimeout, as_completed
from dataclasses import dataclass, replace
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import random
import shutil
import signal
import subprocess
import threading
import time
from typing import Any, Callable, Iterable, TypeVar

from evals.quality.coding_cases import (
    CodingCaseError,
    assert_oracle_isolated_worktree,
    load_cases,
    materialize_clean_worktree,
    provision_validator_dependencies,
    remove_worktree,
    run_validator,
    validator_dependency_lockfile,
)
from evals.quality.coding_runner import CodingRunError, coding_bundle_capabilities, prepare_cache

from .contract import BASELINES, STATIC_BASELINES, ContractError, RESULT_SCHEMA, canonical, digest, file_digest, validate_result
from .report import write_baseline_report, write_reports, write_static_report


ROOT = Path(__file__).resolve().parents[2]
SDK = ROOT / "evals" / "pi_shootout" / "sdk"
DEFAULT_MODEL = "deepseek/deepseek-v4-flash-0731"
DEFAULT_THINKING = "high"
DEFAULT_TIMEOUT_SECONDS = 900
HARD_TIMEOUT_SECONDS = 1800
TASK_TIMEOUT_SECONDS = {
    "express-3936-medium": DEFAULT_TIMEOUT_SECONDS,
    "express-4205-hard": HARD_TIMEOUT_SECONDS,
}
MAX_LOG_BYTES = 256 * 1024
STOP_REQUEST_FILENAME = "stop-request.json"
STOP_TARGET_FILENAME = "stop-target.json"
STOP_REQUEST_SCHEMA = "tea-pi-shootout-stop-request/v1"
STOP_TARGET_SCHEMA = "tea-pi-shootout-stop-target/v1"
EXCLUSION_SCHEMA = "tea-pi-shootout-attempt-exclusion/v1"
FINALIZATION_SCHEMA = "tea-pi-shootout-process-finalization/v1"
STOP_POLL_SECONDS = 0.25
PROCESS_DRAIN_SECONDS = 5
OPERATOR_STOP_REASONS = (
    "operator-requested",
    "diagnostic-bounded",
    "diagnostic-superseded",
)
# The adapter receives the scored task deadline unchanged.  The runner keeps a
# short additional interval only for a terminal adapter timeout to flush its
# result and direct-request witness before forced process-group cleanup.
FINALIZATION_GRACE_SECONDS = 15
SUPPORTED_TASKS = ("express-3936-medium", "express-4205-hard")
# This is intentionally an explicit shared policy rather than a Tea production
# default. It keeps both native harnesses eligible for the same OpenRouter
# parameter-capable routes without pretending that OpenRouter defaults are a
# controlled condition.
ROUTING_POLICY: dict[str, Any] = {"require_parameters": True}
SMOKE_REPEATS = 3
SERIOUS_REPEATS = 7
RepeatResult = TypeVar("RepeatResult")


class ShootoutError(RuntimeError):
    """A configuration or infrastructure boundary failed."""


class _RunCancellation(Exception):
    """An internal worker cancellation requested after a controller interrupt."""


def _raise_if_cancelled(cancellation: threading.Event | None) -> None:
    """Keep cancellation explicit at every runner-owned lane boundary."""
    if cancellation is not None and cancellation.is_set():
        raise _RunCancellation()


@dataclass(frozen=True)
class ProcessFinalization:
    """Evidence about process settlement after the adapter leader exits.

    A terminal adapter result is not enough to score an attempt: a descendant
    may still hold the runner's stdout/stderr pipes or modify the workspace.
    These fields deliberately retain only process-lifecycle facts, never a
    command or process output.
    """

    status: str = "settled"
    stdout_complete: bool = True
    stderr_complete: bool = True
    session_observation_available: bool = True
    session_groups_before_cleanup: tuple[int, ...] = ()
    session_groups_after_cleanup: tuple[int, ...] = ()
    forced_kill: bool = False


@dataclass(frozen=True)
class ProcessOutcome:
    """One child-process outcome, including a controller-recognized stop."""

    exit_code: int | None
    timed_out: bool
    stdout: str
    stderr: str
    elapsed_ms: int
    pid: int
    operator_stop: dict[str, Any] | None = None
    stop_protocol_error: str | None = None
    stop_escalated_to_kill: bool = False
    stop_observed_at: str | None = None
    finalization: ProcessFinalization = ProcessFinalization()


@dataclass(frozen=True)
class AttemptOutcome:
    """A completed benchmark attempt or an excluded Tea-only diagnostic lane."""

    kind: str
    record: dict[str, Any] | None = None
    exclusion: dict[str, Any] | None = None

    def __post_init__(self) -> None:
        if self.kind == "completed" and self.record is not None and self.exclusion is None:
            return
        if self.kind == "excluded" and self.record is None and self.exclusion is not None:
            return
        raise ValueError("attempt outcome must contain exactly its declared completed record or exclusion")

    @classmethod
    def completed(cls, record: dict[str, Any]) -> "AttemptOutcome":
        return cls("completed", record=record)

    @classmethod
    def excluded(cls, exclusion: dict[str, Any]) -> "AttemptOutcome":
        return cls("excluded", exclusion=exclusion)


@dataclass(frozen=True)
class Config:
    task: str
    provider: str
    model: str
    thinking: str
    max_output_tokens: int | None
    repeats: int
    seed: int
    cache_root: Path
    workspace_root: Path
    out: Path
    # High-thinking, uncapped completions need enough time to finish an actual
    # coding task. This remains one identical per-attempt budget for every
    # condition and is excluded from agent-token accounting. Zero is an
    # explicit diagnostic mode: the runner does not impose an outer wall clock.
    timeout_seconds: int | None = None
    keep_worktrees: bool = False
    static_only: bool = False
    # A Tea-only run is a single-baseline diagnostic that persists the same
    # attempt evidence without pretending a paired Pi comparison exists.
    tea_only: bool = False
    # Repeats are independent experimental lanes. The default intentionally
    # starts every requested lane at once; condition order stays sequential
    # within each lane so counterbalancing is still meaningful.
    parallel_repeats: int | None = None
    # An OS-enforced Tea shell-child boundary is available only for explicitly
    # Tea-only diagnostics. A paired result would require the same boundary
    # for Pi before it could remain comparable.
    tool_child_sandbox: str = "none"
    # A targeted correction is model-visible continuation policy. It is useful
    # to screen Tea's invalid-edit recovery, but never belongs in a paired
    # comparison until Pi has the same behavior.
    edit_recovery_projection: str = "none"
    # The direct and source-local workflow gates block pre-edit exploration.
    # Both static adapters implement them as fresh-attempt policy, so they are
    # eligible for a controlled static comparison but never for tea-jit.
    pre_edit_tool_gate: str = "none"
    # This paired source-local condition resets after each later successful
    # native edit result and admits only a direct foreground bash child with
    # content-free evidence that it exited zero.
    # It is a workflow witness, not a claim about which task check was chosen.
    post_edit_validation_gate: str = "none"
    # A static model-prompt candidate is explicit and evidence-bearing. It
    # preserves Tea's four tool definitions and authority while changing only
    # the selected static prompt section.
    static_prompt_profile: str = "builtin-v1"

    def __post_init__(self) -> None:
        if self.timeout_seconds is None:
            object.__setattr__(self, "timeout_seconds", TASK_TIMEOUT_SECONDS.get(self.task, DEFAULT_TIMEOUT_SECONDS))
        if self.tea_only and not self.static_only:
            object.__setattr__(self, "static_only", True)

    def validate(self) -> None:
        if self.task not in SUPPORTED_TASKS:
            raise ShootoutError(f"pi-shootout supports only {', '.join(SUPPORTED_TASKS)}")
        if self.provider != "openrouter":
            raise ShootoutError("pi-shootout v0 supports only provider openrouter")
        if self.model != DEFAULT_MODEL:
            raise ShootoutError(f"pi-shootout v0 requires model {DEFAULT_MODEL!r}, not {self.model!r}")
        if self.thinking != DEFAULT_THINKING:
            raise ShootoutError(f"pi-shootout v0 requires thinking level {DEFAULT_THINKING!r}")
        if self.max_output_tokens is not None:
            raise ShootoutError("pi-shootout v0 requires unlimited max output tokens")
        if not isinstance(self.repeats, int) or self.repeats < 1:
            raise ShootoutError("repeats must be positive")
        if not isinstance(self.seed, int):
            raise ShootoutError("seed must be an integer")
        if not isinstance(self.timeout_seconds, int) or self.timeout_seconds < 0:
            raise ShootoutError("attempt timeout must be a non-negative integer (zero disables the outer wall clock)")
        if self.parallel_repeats is not None and (
            not isinstance(self.parallel_repeats, int)
            or self.parallel_repeats < 1
            or self.parallel_repeats > self.repeats
        ):
            raise ShootoutError("parallel_repeats must be between one and repeats")
        if self.tool_child_sandbox not in {"none", "macos-seatbelt-v1", "macos-seatbelt-v2"}:
            raise ShootoutError(
                "tool_child_sandbox must be none, macos-seatbelt-v1, or macos-seatbelt-v2"
            )
        if self.tool_child_sandbox != "none" and not self.tea_only:
            raise ShootoutError(
                "tool_child_sandbox is a Tea-only diagnostic policy and cannot enter a paired comparison"
            )
        if self.edit_recovery_projection not in {"none", "canonical-v1"}:
            raise ShootoutError("edit_recovery_projection must be none or canonical-v1")
        if self.edit_recovery_projection != "none" and not self.tea_only:
            raise ShootoutError(
                "edit_recovery_projection is a Tea-only diagnostic policy and cannot enter a paired comparison"
            )
        if self.pre_edit_tool_gate not in {"none", "direct-edit-v1", "source-local-v1"}:
            raise ShootoutError("pre_edit_tool_gate must be none, direct-edit-v1, or source-local-v1")
        if self.pre_edit_tool_gate != "none" and not self.static_only:
            raise ShootoutError(
                "pre_edit_tool_gate is a fresh static-only policy and cannot enter a tea-jit run"
            )
        if self.pre_edit_tool_gate == "source-local-v1" and self.tea_only:
            raise ShootoutError(
                "source-local-v1 is a fresh static paired policy and cannot enter a Tea-only diagnostic"
            )
        if self.post_edit_validation_gate not in {"none", "unmasked-evidence-v1"}:
            raise ShootoutError("post_edit_validation_gate must be none or unmasked-evidence-v1")
        if self.post_edit_validation_gate != "none" and not self.static_only:
            raise ShootoutError(
                "post_edit_validation_gate is a fresh static-only policy and cannot enter a tea-jit run"
            )
        if self.post_edit_validation_gate != "none" and self.tea_only:
            raise ShootoutError(
                "post_edit_validation_gate is a fresh paired policy and cannot enter a Tea-only diagnostic"
            )
        if (
            self.post_edit_validation_gate == "unmasked-evidence-v1"
            and self.pre_edit_tool_gate != "source-local-v1"
        ):
            raise ShootoutError(
                "unmasked-evidence-v1 requires pre_edit_tool_gate source-local-v1"
            )
        if self.static_prompt_profile not in {
            "builtin-v1",
            "no-history-v1",
            "prefix-guard-v1",
            "prefix-guard-focused-v1",
        }:
            raise ShootoutError(
                "static_prompt_profile must be builtin-v1, no-history-v1, prefix-guard-v1, or prefix-guard-focused-v1"
            )
        if self.static_prompt_profile != "builtin-v1" and not self.tea_only:
            raise ShootoutError(
                "non-default static_prompt_profile is a Tea-only diagnostic policy and cannot enter a paired comparison"
            )
        if self.static_prompt_profile == "no-history-v1" and not self.static_only:
            raise ShootoutError("non-default static_prompt_profile requires static_only")

    def effective_parallel_repeats(self) -> int:
        return self.repeats if self.parallel_repeats is None else self.parallel_repeats


def selected_case(task_id: str) -> dict[str, Any]:
    cases = {case["id"]: case for case in load_cases()}
    try:
        return cases[task_id]
    except KeyError as error:
        raise ShootoutError(f"unknown coding case {task_id!r}") from error


def capability_manifest() -> list[dict[str, Any]]:
    """Use the shared checked-in Luau coding-bundle contract for both adapters."""
    try:
        return coding_bundle_capabilities()
    except CodingRunError as error:
        raise ShootoutError(str(error)) from error


def adapter_task(case: dict[str, Any], capabilities: list[dict[str, Any]], timeout_seconds: int) -> dict[str, Any]:
    source_local = source_local_task_metadata(case)
    return {
        "schema_version": "tea-coding-eval-task/v1",
        "task_id": case["id"],
        "task_version": 1,
        "kind": "coding",
        "prompt": case["task"]["prompt"],
        "source_local_v1": source_local,
        "initial_workspace": [],
        "capabilities": capabilities,
        "timeout_seconds": timeout_seconds,
        "oracle_id": "quality-express-validator-v1",
    }


def source_local_task_metadata(case: dict[str, Any]) -> dict[str, Any]:
    """Copy the checked-in versioned target declaration into an adapter task."""
    metadata = case["task"].get("source_local_v1")
    if not isinstance(metadata, dict):
        raise ShootoutError("coding case is missing source-local-v1 task metadata")
    schema_version = metadata.get("schema_version")
    targets = metadata.get("targets")
    if schema_version != "tea-coding-eval-source-local/v1":
        raise ShootoutError("coding case has an unsupported source-local task metadata version")
    if not isinstance(targets, list) or not targets or any(not isinstance(target, str) for target in targets):
        raise ShootoutError("coding case has invalid source-local task targets")
    return {"schema_version": schema_version, "targets": list(targets)}


def source_local_targets(case: dict[str, Any]) -> list[str]:
    return list(source_local_task_metadata(case)["targets"])


def configured_source_local_targets(config: Config, case: dict[str, Any]) -> list[str]:
    return source_local_targets(case) if config.pre_edit_tool_gate == "source-local-v1" else []


def assert_source_local_targets_in_clean_workspace(case: dict[str, Any], workspace: Path) -> list[str]:
    """Confirm every declared target exists in the fresh clean checkout.

    ``initial_workspace_state`` establishes that the worktree is clean before
    this call. This adds the target-local part of that pre-inference witness
    without accepting a path supplied by an adapter or model.
    """
    targets = source_local_targets(case)
    for target in targets:
        candidate = workspace / target
        if candidate.is_symlink() or not candidate.is_file():
            raise ShootoutError(f"declared source-local target is not a regular workspace file: {target}")
    return targets


def randomized_plan(repeats: int, seed: int, baselines: tuple[str, ...] = BASELINES) -> list[list[str]]:
    """Return a seed-reproducible, counterbalanced sequential schedule.

    Static pairs alternate AB/BA in balanced blocks. Three-condition runs use
    the six Williams-style orders, so positions and immediate predecessors are
    balanced over each complete block rather than relying on random luck.
    """
    if repeats < 1:
        raise ShootoutError("repeats must be positive")
    randomizer = random.Random(seed)
    if baselines == ("tea-static",):
        return [["tea-static"] for _ in range(repeats)]
    if baselines == STATIC_BASELINES:
        orders = [list(STATIC_BASELINES), list(reversed(STATIC_BASELINES))]
    elif baselines == BASELINES:
        first, second, third = BASELINES
        orders = [
            [first, second, third], [third, second, first],
            [second, third, first], [first, third, second],
            [third, first, second], [second, first, third],
        ]
    else:
        raise ShootoutError("counterbalanced schedule only supports the pinned shootout conditions")
    # A seeded rotation changes which balanced order is first, while every
    # complete block retains the same balance invariant.
    offset = randomizer.randrange(len(orders))
    rotated = orders[offset:] + orders[:offset]
    return [list(rotated[index % len(rotated)]) for index in range(repeats)]


def run_repeat_lanes(
    orders: list[list[str]],
    parallel_repeats: int,
    run_repeat: Callable[[int, list[str], threading.Event], RepeatResult],
) -> list[RepeatResult]:
    """Run independent repeats concurrently while preserving per-repeat order.

    ``run_repeat`` owns every workspace, evidence directory, dependency tree,
    and child process for its lane. It receives a shared cancellation event so
    a raw controller interrupt can settle live worker-owned processes before
    this function re-raises. Results are returned in repeat order rather than
    completion order, making persisted artifacts deterministic even when
    provider latency differs across the parallel lanes.
    """
    if not orders or parallel_repeats < 1 or parallel_repeats > len(orders):
        raise ShootoutError("parallel repeat lane count must be between one and the number of repeats")
    cancellation = threading.Event()
    if parallel_repeats == 1:
        try:
            return [run_repeat(repeat, order, cancellation) for repeat, order in enumerate(orders)]
        except BaseException:
            cancellation.set()
            raise
    results: dict[int, RepeatResult] = {}
    executor = ThreadPoolExecutor(max_workers=parallel_repeats, thread_name_prefix="tea-shootout-repeat")
    pending: dict[Any, int] = {}
    try:
        for repeat, order in enumerate(orders):
            pending[executor.submit(run_repeat, repeat, order, cancellation)] = repeat
        remaining = set(pending)
        while remaining:
            try:
                future = next(as_completed(remaining, timeout=STOP_POLL_SECONDS))
            except FuturesTimeout:
                # A bounded wait gives the main thread a chance to receive a
                # raw controller interrupt and signal every worker promptly.
                continue
            results[pending[future]] = future.result()
            remaining.remove(future)
    except BaseException:
        # ThreadPoolExecutor cannot forcibly terminate a Python worker. Its
        # explicit event lets runner-owned workers stop their active adapter
        # process and prevents queued repeat lanes from starting.
        cancellation.set()
        for future in pending:
            future.cancel()
        raise
    finally:
        executor.shutdown(wait=True, cancel_futures=True)
    return [results[repeat] for repeat in range(len(orders))]


def _sha256_file(path: Path) -> str:
    digest_value = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest_value.update(chunk)
    return digest_value.hexdigest()


def toolchain_manifest(environment: dict[str, str] | None = None) -> dict[str, Any]:
    """Fingerprint only executables that can materially affect this task.

    It deliberately does not serialize arbitrary parent environment values.
    """
    path = (environment or os.environ).get("PATH", "")
    if not path:
        raise ShootoutError("PATH is unavailable for toolchain fingerprinting")
    entries: list[dict[str, Any]] = []
    for name in ("bash", "git", "curl", "node", "npm"):
        resolved = shutil.which(name, path=path)
        if not resolved:
            raise ShootoutError(f"required toolchain executable is unavailable: {name}")
        executable = Path(resolved).resolve()
        try:
            version = subprocess.run([str(executable), "--version"], env={"PATH": path, "LANG": "C", "LC_ALL": "C"}, text=True, capture_output=True, timeout=10, check=False)
            version_text = (version.stdout or version.stderr).strip().splitlines()[0] if (version.stdout or version.stderr).strip() else None
        except (OSError, subprocess.SubprocessError):
            version_text = None
        entries.append({"name": name, "path": str(executable), "sha256": _sha256_file(executable), "version": version_text})
    manifest = {"schema_version": "tea-pi-toolchain-manifest/v1", "executables": entries}
    return manifest | {"sha256": digest(manifest)}


def initial_workspace_state(workspace: Path) -> dict[str, str]:
    """Fail before inference if an attempt is not the clean pinned checkout."""
    status = subprocess.run(["git", "status", "--porcelain=v1", "--untracked-files=all"], cwd=workspace, text=True, capture_output=True, check=False)
    if status.returncode or status.stdout:
        raise ShootoutError("attempt workspace is not clean before adapter start")
    tree = subprocess.run(["git", "ls-files", "-s"], cwd=workspace, text=True, capture_output=True, check=False)
    if tree.returncode:
        raise ShootoutError("cannot fingerprint initial workspace tree")
    return {"commit": subprocess.run(["git", "rev-parse", "HEAD"], cwd=workspace, text=True, capture_output=True, check=True).stdout.strip(), "tree_sha256": hashlib.sha256(tree.stdout.encode()).hexdigest()}


def normalized_environment(
    *,
    home: Path,
    temporary: Path,
    npm_cache: Path | None = None,
    node_path: Path | None = None,
) -> dict[str, str]:
    """The only child-shell environment; it deliberately has no inherited credential."""
    path = os.environ.get("PATH", "")
    if not path:
        raise ShootoutError("PATH is unavailable for a sanitized coding-tool environment")
    environment = {
        "PATH": path,
        "HOME": str(home),
        "TMPDIR": str(temporary),
        "LANG": "C",
        "LC_ALL": "C",
        "NPM_CONFIG_AUDIT": "false",
        "NPM_CONFIG_FUND": "false",
    }
    if npm_cache is not None:
        environment["npm_config_cache"] = str(npm_cache)
        environment["NPM_CONFIG_OFFLINE"] = "true"
    if node_path is not None:
        environment["NODE_PATH"] = str(node_path)
    return environment


def _replace_attempt_paths(
    value: str,
    *,
    workspace: Path,
    home: Path,
    temporary: Path,
    npm_cache: Path | None,
    node_path: Path | None,
) -> str:
    replacements = [(str(workspace), "{WORKSPACE}"), (str(home), "{HOME}"), (str(temporary), "{TMPDIR}")]
    if npm_cache is not None:
        replacements.append((str(npm_cache), "{NPM_CACHE}"))
    if node_path is not None:
        replacements.append((str(node_path), "{NODE_PATH}"))
    for source, target in replacements:
        value = value.replace(source, target)
    return value


def shell_environment_digest(
    environment: dict[str, str],
    *,
    workspace: Path,
    home: Path,
    temporary: Path,
    npm_cache: Path | None,
    node_path: Path | None,
) -> str:
    public = {
        name: _replace_attempt_paths(
            value,
            workspace=workspace,
            home=home,
            temporary=temporary,
            npm_cache=npm_cache,
            node_path=node_path,
        )
        for name, value in sorted(environment.items())
    }
    return digest(public)


def check_curl(environment: dict[str, str], cwd: Path) -> bool:
    completed = subprocess.run(["bash", "-c", "command -v curl"], cwd=cwd, env=environment, text=True, capture_output=True, check=False)
    return completed.returncode == 0


def _bounded(text: str) -> str:
    encoded = text.encode("utf-8", errors="replace")
    if len(encoded) <= MAX_LOG_BYTES:
        return text
    return encoded[:MAX_LOG_BYTES].decode("utf-8", errors="replace") + "\n[truncated]\n"


def _write_process_finalization(attempt_directory: Path, process: ProcessOutcome) -> None:
    """Persist lifecycle-only evidence when an adapter process did not settle."""
    finalization = process.finalization
    if finalization.status == "settled":
        return
    payload = {
        "schema_version": FINALIZATION_SCHEMA,
        "status": finalization.status,
        "leader": {"pid": process.pid, "exit_code": process.exit_code},
        "streams": {
            "stdout_complete": finalization.stdout_complete,
            "stderr_complete": finalization.stderr_complete,
        },
        "session": {
            "observation_available": finalization.session_observation_available,
            "groups_before_cleanup": list(finalization.session_groups_before_cleanup),
            "groups_after_cleanup": list(finalization.session_groups_after_cleanup),
        },
        "forced_kill": finalization.forced_kill,
    }
    (attempt_directory / "process-finalization.json").write_text(
        json.dumps(payload, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def _require_settled_process(baseline: str, process: ProcessOutcome) -> None:
    """Reject process evidence that cannot prove the adapter lane has settled."""
    if process.finalization.status != "settled":
        raise ShootoutError(
            f"{baseline} infrastructure failure: process finalization={process.finalization.status}"
        )


def _utc_timestamp() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def _attempt_identity(value: Any, *, filename: str) -> tuple[str, str, int]:
    if not isinstance(value, dict):
        raise ShootoutError(f"{filename} must contain an object")
    attempt_id, baseline, repeat = value.get("attempt_id"), value.get("baseline_id"), value.get("repeat_lane")
    if not isinstance(attempt_id, str) or not attempt_id:
        raise ShootoutError(f"{filename} must contain a non-empty attempt_id")
    if not isinstance(baseline, str) or not baseline:
        raise ShootoutError(f"{filename} must contain a non-empty baseline_id")
    if not isinstance(repeat, int) or repeat < 1:
        raise ShootoutError(f"{filename} must contain a positive repeat_lane")
    return attempt_id, baseline, repeat


def _read_stop_target(attempt_directory: Path) -> dict[str, Any]:
    path = attempt_directory / STOP_TARGET_FILENAME
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise ShootoutError(f"cannot read operator stop target {path}: {error}") from error
    if not isinstance(value, dict) or value.get("schema_version") != STOP_TARGET_SCHEMA:
        raise ShootoutError(f"{path} is not a {STOP_TARGET_SCHEMA} target")
    _attempt_identity(value, filename=STOP_TARGET_FILENAME)
    return value


def _write_json_once(path: Path, value: dict[str, Any]) -> None:
    """Publish a small controller sidecar without exposing a partial JSON file."""
    temporary = path.with_name(f".{path.name}.{os.getpid()}.{time.monotonic_ns()}.tmp")
    try:
        with temporary.open("x", encoding="utf-8") as destination:
            json.dump(value, destination, indent=2, sort_keys=True)
            destination.write("\n")
            destination.flush()
            os.fsync(destination.fileno())
        try:
            # Linking is an atomic create-if-absent operation on the attempt
            # filesystem. `replace` would let a second operator overwrite the
            # reason or target of an already accepted request.
            os.link(temporary, path)
        except FileExistsError as error:
            raise ShootoutError(f"operator stop request already exists at {path}") from error
    finally:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass


def _write_stop_request(attempt_directory: Path, reason: str) -> dict[str, Any]:
    """Atomically request a controller stop for one eligible Tea-only lane."""
    if reason not in OPERATOR_STOP_REASONS:
        raise ShootoutError(f"operator stop reason must be one of {', '.join(OPERATOR_STOP_REASONS)}")
    target = _read_stop_target(attempt_directory)
    operator_stop = target.get("operator_stop")
    if not isinstance(operator_stop, dict) or operator_stop.get("eligible") is not True:
        raise ShootoutError("operator stop is available only for Tea-only diagnostic attempts")
    attempt_id, baseline, repeat = _attempt_identity(target, filename=STOP_TARGET_FILENAME)
    request = {
        "schema_version": STOP_REQUEST_SCHEMA,
        "attempt_id": attempt_id,
        "baseline_id": baseline,
        "repeat_lane": repeat,
        "reason": reason,
        "requested_at": _utc_timestamp(),
    }
    _write_json_once(attempt_directory / STOP_REQUEST_FILENAME, request)
    return request


def _read_stop_request(path: Path, target: dict[str, Any]) -> dict[str, Any] | None:
    """Return a valid request or reject a malformed/mis-targeted sidecar."""
    if not path.exists():
        return None
    try:
        request = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise ShootoutError(f"invalid operator stop request {path}: {error}") from error
    if not isinstance(request, dict) or request.get("schema_version") != STOP_REQUEST_SCHEMA:
        raise ShootoutError(f"invalid operator stop request {path}: expected {STOP_REQUEST_SCHEMA}")
    request_identity = _attempt_identity(request, filename=STOP_REQUEST_FILENAME)
    target_identity = _attempt_identity(target, filename=STOP_TARGET_FILENAME)
    if request_identity != target_identity:
        raise ShootoutError(f"invalid operator stop request {path}: target does not match this attempt")
    if request.get("reason") not in OPERATOR_STOP_REASONS:
        raise ShootoutError(f"invalid operator stop request {path}: unsupported reason")
    if not isinstance(request.get("requested_at"), str) or not request["requested_at"]:
        raise ShootoutError(f"invalid operator stop request {path}: requested_at is required")
    return request


def attempt_hard_timeout_seconds(baseline: str, timeout_seconds: int) -> int:
    """Reserve static-adapter evidence-finalization time without extending model work."""
    if timeout_seconds == 0 or baseline not in STATIC_BASELINES:
        return timeout_seconds
    return timeout_seconds + FINALIZATION_GRACE_SECONDS


def _attempt_process_groups(root_pid: int) -> list[int]:
    """Return an attempt's process groups, deepest nested group first.

    The adapter itself starts in a dedicated session, but Tea's shell runner
    intentionally gives each model-issued command its own process group for
    its cancellation contract. A controller timeout or interrupt must stop
    those nested groups before it stops the adapter group; otherwise the shell
    command can be reparented and continue outside its scored lane.
    """
    if os.name != "posix":
        return [root_pid]
    try:
        completed = subprocess.run(
            ["ps", "-axo", "pid=,ppid=,pgid="],
            text=True,
            capture_output=True,
            check=False,
        )
    except OSError:
        # The root group remains a safe conservative fallback if the host
        # cannot provide a process table.
        return [root_pid]
    if completed.returncode != 0:
        return [root_pid]

    processes: dict[int, tuple[int, int]] = {}
    children: dict[int, list[int]] = {}
    for line in completed.stdout.splitlines():
        columns = line.split()
        if len(columns) != 3:
            continue
        try:
            pid, parent_pid, process_group = (int(column) for column in columns)
        except ValueError:
            continue
        processes[pid] = (parent_pid, process_group)
        children.setdefault(parent_pid, []).append(pid)

    root_group = processes.get(root_pid, (0, root_pid))[1]
    group_depths = {root_group: 0}
    pending = [(root_pid, 0)]
    visited: set[int] = set()
    while pending:
        pid, depth = pending.pop()
        if pid in visited:
            continue
        visited.add(pid)
        for child_pid in children.get(pid, []):
            child_group = processes[child_pid][1]
            group_depths[child_group] = max(group_depths.get(child_group, 0), depth + 1)
            pending.append((child_pid, depth + 1))

    nested_groups = [
        process_group
        for process_group, _depth in sorted(
            (
                (process_group, depth)
                for process_group, depth in group_depths.items()
                if process_group != root_group
            ),
            key=lambda item: (item[1], item[0]),
            reverse=True,
        )
    ]
    return [*nested_groups, root_group]


def _attempt_session_groups(session_id: int) -> tuple[int, ...] | None:
    """Return every live process group in one runner-owned POSIX session.

    PPID ancestry disappears when the adapter leader exits.  Its session stays
    meaningful for ordinary descendants, including Tea's intentionally nested
    command groups, until the last member exits.  ``None`` is intentionally
    distinct from an empty tuple: an unavailable process table cannot prove
    the session was settled.
    """
    if os.name != "posix":
        return ()
    try:
        completed = subprocess.run(
            ["ps", "-axo", "pid=,pgid="],
            text=True,
            capture_output=True,
            check=False,
        )
    except OSError:
        return None
    if completed.returncode != 0:
        return None
    groups: set[int] = set()
    for line in completed.stdout.splitlines():
        columns = line.split()
        if len(columns) != 2:
            continue
        try:
            process_id, process_group = (int(column) for column in columns)
        except ValueError:
            continue
        try:
            observed_session = os.getsid(process_id)
        except ProcessLookupError:
            continue
        except OSError:
            return None
        if observed_session == session_id:
            groups.add(process_group)
    return tuple(sorted(groups))


def _signal_process_groups(process_groups: Iterable[int], signal_number: signal.Signals) -> None:
    """Signal the supplied attempt-owned groups, tolerating normal exit races."""
    for process_group in process_groups:
        try:
            os.killpg(process_group, signal_number)
        except ProcessLookupError:
            pass


def _settle_attempt_session(session_id: int) -> tuple[tuple[int, ...] | None, tuple[int, ...] | None]:
    """Kill and boundedly observe descendants after their leader has exited."""
    before = _attempt_session_groups(session_id)
    if before is None:
        return None, None
    if not before:
        return before, before
    _signal_process_groups(before, signal.SIGKILL)
    deadline = time.monotonic() + 5
    after = before
    while True:
        observed = _attempt_session_groups(session_id)
        if observed is None:
            return before, None
        after = observed
        if not after or time.monotonic() >= deadline:
            return before, after
        time.sleep(0.05)


def _signal_attempt_process_group(process: subprocess.Popen[str], signal_number: signal.Signals) -> None:
    """Signal one attempt and its nested shell groups without touching siblings."""
    try:
        if os.name == "posix":
            process_groups = _attempt_process_groups(process.pid)
            for index, process_group in enumerate(process_groups):
                # Let the adapter root receive the requested graceful signal so
                # it can retain terminal evidence. A nested shell group has no
                # such responsibility: kill it immediately so a model command
                # cannot trap SIGTERM and outlive its scored attempt.
                group_signal = (
                    signal.SIGKILL
                    if signal_number == signal.SIGTERM and index + 1 < len(process_groups)
                    else signal_number
                )
                try:
                    os.killpg(process_group, group_signal)
                except ProcessLookupError:
                    # A child may exit between the process-table snapshot and
                    # its group signal. Continue with the rest of this lane.
                    pass
        elif signal_number == signal.SIGTERM:
            process.terminate()
        else:
            process.kill()
    except ProcessLookupError:
        # The adapter may exit between `communicate` raising and cleanup.
        pass


def _timeout_output(value: str | bytes | None) -> str:
    """Normalize the partial transcript Python attaches to a timeout."""
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace")
    return value


def _close_process_streams(process: subprocess.Popen[str]) -> None:
    """Release local pipe readers after recording a bounded partial transcript."""
    for stream in (process.stdout, process.stderr):
        if stream is not None:
            try:
                stream.close()
            except OSError:
                pass


def _finalize_exited_process(
    process: subprocess.Popen[str],
    *,
    stdout: str | None = None,
    stderr: str | None = None,
    pipe_was_open: bool = False,
) -> tuple[str, str, ProcessFinalization]:
    """Boundedly collect an exited leader and conservatively settle its session.

    ``communicate`` reaching EOF is not sufficient on POSIX: a descendant can
    have closed its streams while still working in a nested process group.
    Conversely, an open inherited stream proves the root exit was not a clean
    completion even if it closes during this short cleanup interval.
    """
    session_before: tuple[int, ...] | None = ()
    session_after: tuple[int, ...] | None = ()
    if os.name == "posix":
        session_before, session_after = _settle_attempt_session(process.pid)

    stdout_complete = stdout is not None
    stderr_complete = stderr is not None
    if stdout is None or stderr is None:
        try:
            collected_stdout, collected_stderr = process.communicate(timeout=PROCESS_DRAIN_SECONDS)
            stdout = collected_stdout
            stderr = collected_stderr
            stdout_complete = True
            stderr_complete = True
        except subprocess.TimeoutExpired as error:
            pipe_was_open = True
            stdout = _timeout_output(error.stdout)
            stderr = _timeout_output(error.stderr)
            _close_process_streams(process)

    if session_before is None or session_after is None:
        status = "post-exit-session-unproven"
    elif session_before:
        status = "post-exit-descendants-cleaned" if not session_after else "post-exit-descendants-unsettled"
    elif pipe_was_open:
        status = "post-exit-pipe-open"
    else:
        status = "settled"
    return (
        stdout or "",
        stderr or "",
        ProcessFinalization(
            status=status,
            stdout_complete=stdout_complete,
            stderr_complete=stderr_complete,
            session_observation_available=session_before is not None and session_after is not None,
            session_groups_before_cleanup=session_before or (),
            session_groups_after_cleanup=session_after or (),
        ),
    )


def _outcome(
    process: subprocess.Popen[str],
    *,
    started: int,
    exit_code: int | None,
    timed_out: bool,
    stdout: str,
    stderr: str,
    finalization: ProcessFinalization,
) -> ProcessOutcome:
    return ProcessOutcome(
        exit_code=exit_code,
        timed_out=timed_out,
        stdout=_bounded(stdout),
        stderr=_bounded(stderr),
        elapsed_ms=(time.monotonic_ns() - started) // 1_000_000,
        pid=process.pid,
        finalization=finalization,
    )


def _stop_process_and_collect(process: subprocess.Popen[str], *, started: int, timed_out: bool) -> ProcessOutcome:
    """Boundedly stop one attempt and retain its available process evidence."""
    _signal_attempt_process_group(process, signal.SIGTERM)
    try:
        stdout, stderr = process.communicate(timeout=PROCESS_DRAIN_SECONDS)
        stdout, stderr, finalization = _finalize_exited_process(process, stdout=stdout, stderr=stderr)
        return _outcome(
            process,
            started=started,
            exit_code=None if timed_out else process.returncode,
            timed_out=timed_out,
            stdout=stdout,
            stderr=stderr,
            finalization=finalization,
        )
    except subprocess.TimeoutExpired:
        _signal_attempt_process_group(process, signal.SIGKILL)
        stdout, stderr, finalization = _finalize_exited_process(process, pipe_was_open=True)
        finalization = replace(finalization, forced_kill=True)
        return _outcome(
            process,
            started=started,
            exit_code=None if timed_out else process.returncode,
            timed_out=timed_out,
            stdout=stdout,
            stderr=stderr,
            finalization=finalization,
        )


def _settle_interrupted_attempt(process: subprocess.Popen[str], *, started: int) -> None:
    """Boundedly stop an attempt before propagating a raw controller interrupt.

    A raw interrupt is not an operator-stop outcome, but it must still use the
    normal TERM-then-KILL containment path. If another interrupt or cleanup
    failure occurs while draining, make one best-effort KILL attempt without
    replacing the original controller exception.
    """
    try:
        _stop_process_and_collect(process, started=started, timed_out=False)
    except BaseException:
        _signal_attempt_process_group(process, signal.SIGKILL)


def _run_process(
    command: list[str],
    *,
    cwd: Path,
    environment: dict[str, str],
    timeout_seconds: int,
    cancellation: threading.Event | None = None,
) -> ProcessOutcome:
    """Run an adapter without treating an exited pipe-holding leader as complete."""
    started = time.monotonic_ns()
    process = subprocess.Popen(
        command,
        cwd=cwd,
        env=environment,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=os.name == "posix",
    )
    deadline = None if timeout_seconds == 0 else time.monotonic() + timeout_seconds
    try:
        while True:
            if cancellation is not None and cancellation.is_set():
                _settle_interrupted_attempt(process, started=started)
                raise _RunCancellation()
            remaining = None if deadline is None else deadline - time.monotonic()
            if remaining is not None and remaining <= 0:
                return _stop_process_and_collect(process, started=started, timed_out=True)
            wait_seconds = STOP_POLL_SECONDS if remaining is None else min(STOP_POLL_SECONDS, remaining)
            try:
                stdout, stderr = process.communicate(timeout=wait_seconds)
                stdout, stderr, finalization = _finalize_exited_process(process, stdout=stdout, stderr=stderr)
                return _outcome(
                    process,
                    started=started,
                    exit_code=process.returncode,
                    timed_out=False,
                    stdout=stdout,
                    stderr=stderr,
                    finalization=finalization,
                )
            except subprocess.TimeoutExpired:
                if process.poll() is not None:
                    stdout, stderr, finalization = _finalize_exited_process(process, pipe_was_open=True)
                    return _outcome(
                        process,
                        started=started,
                        exit_code=process.returncode,
                        timed_out=False,
                        stdout=stdout,
                        stderr=stderr,
                        finalization=finalization,
                    )
                continue
    except _RunCancellation:
        raise
    except BaseException:
        # `start_new_session` makes every attempt its own group. Without this
        # cleanup, Ctrl-C in the run controller can orphan a live provider
        # adapter or model-issued shell command while other lanes continue.
        _settle_interrupted_attempt(process, started=started)
        raise


def _run_process_with_stop(
    command: list[str],
    *,
    cwd: Path,
    environment: dict[str, str],
    timeout_seconds: int,
    stop_target: dict[str, Any],
    stop_request_path: Path,
    cancellation: threading.Event | None = None,
) -> ProcessOutcome:
    """Poll one Tea-only lane for an authenticated operator stop request.

    The request is intentionally distinct from an OS signal.  A raw SIGTERM
    reaches the adapter as usual and remains an infrastructure failure when it
    cannot produce a valid result. Only a valid, attempt-specific sidecar can
    produce an excluded diagnostic outcome.
    """
    started = time.monotonic_ns()
    process = subprocess.Popen(
        command,
        cwd=cwd,
        env=environment,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=os.name == "posix",
    )
    deadline = None if timeout_seconds == 0 else time.monotonic() + timeout_seconds

    def finish(
        *,
        exit_code: int | None,
        timed_out: bool,
        stdout: str,
        stderr: str,
        finalization: ProcessFinalization = ProcessFinalization(),
        operator_stop: dict[str, Any] | None = None,
        stop_protocol_error: str | None = None,
        stop_escalated_to_kill: bool = False,
        stop_observed_at: str | None = None,
    ) -> ProcessOutcome:
        return ProcessOutcome(
            exit_code=exit_code,
            timed_out=timed_out,
            stdout=_bounded(stdout),
            stderr=_bounded(stderr),
            elapsed_ms=(time.monotonic_ns() - started) // 1_000_000,
            pid=process.pid,
            operator_stop=operator_stop,
            stop_protocol_error=stop_protocol_error,
            stop_escalated_to_kill=stop_escalated_to_kill,
            stop_observed_at=stop_observed_at,
            finalization=finalization,
        )

    try:
        while True:
            if cancellation is not None and cancellation.is_set():
                _settle_interrupted_attempt(process, started=started)
                raise _RunCancellation()
            # Do not turn a request written after the adapter has naturally
            # finished into an exclusion. The controller stopped no work in
            # that case, so normal result validation must still decide it.
            if process.poll() is not None:
                stdout, stderr, finalization = _finalize_exited_process(process)
                return finish(
                    exit_code=process.returncode,
                    timed_out=False,
                    stdout=stdout,
                    stderr=stderr,
                    finalization=finalization,
                )
            try:
                request = _read_stop_request(stop_request_path, stop_target)
            except ShootoutError as error:
                stopped = _stop_process_and_collect(process, started=started, timed_out=False)
                return finish(
                    exit_code=stopped.exit_code,
                    timed_out=False,
                    stdout=stopped.stdout,
                    stderr=stopped.stderr,
                    finalization=stopped.finalization,
                    stop_protocol_error=str(error),
                    stop_escalated_to_kill=stopped.finalization.forced_kill,
                    stop_observed_at=_utc_timestamp(),
                )
            if request is not None:
                observed_at = _utc_timestamp()
                stopped = _stop_process_and_collect(process, started=started, timed_out=False)
                return finish(
                    exit_code=stopped.exit_code,
                    timed_out=False,
                    stdout=stopped.stdout,
                    stderr=stopped.stderr,
                    finalization=stopped.finalization,
                    operator_stop=request,
                    stop_escalated_to_kill=stopped.finalization.forced_kill,
                    stop_observed_at=observed_at,
                )

            remaining = None if deadline is None else deadline - time.monotonic()
            if remaining is not None and remaining <= 0:
                stopped = _stop_process_and_collect(process, started=started, timed_out=True)
                return finish(
                    exit_code=None,
                    timed_out=True,
                    stdout=stopped.stdout,
                    stderr=stopped.stderr,
                    finalization=stopped.finalization,
                )
            wait_seconds = STOP_POLL_SECONDS if remaining is None else min(STOP_POLL_SECONDS, remaining)
            try:
                stdout, stderr = process.communicate(timeout=wait_seconds)
                stdout, stderr, finalization = _finalize_exited_process(process, stdout=stdout, stderr=stderr)
                return finish(
                    exit_code=process.returncode,
                    timed_out=False,
                    stdout=stdout,
                    stderr=stderr,
                    finalization=finalization,
                )
            except subprocess.TimeoutExpired:
                # The next bounded poll observes either an exact sidecar stop
                # or the ordinary scored timeout.
                continue
    except _RunCancellation:
        raise
    except BaseException:
        _settle_interrupted_attempt(process, started=started)
        raise


def _vault_command(command: list[str]) -> list[str]:
    """Keep key injection at the caller-visible final live child boundary."""
    return ["vault", "OPENROUTER_API_KEY", "--", *command]


def adapter_command(config: Config, baseline: str, *, task: Path, workspace: Path, capabilities: Path, result: Path, evidence: Path, attempt_id: str, shell_environment: dict[str, str]) -> list[str]:
    common = [
        "--task-json", str(task), "--workspace", str(workspace), "--capabilities-json", str(capabilities),
        "--result-json", str(result), "--evidence-dir", str(evidence), "--attempt-id", attempt_id,
        "--baseline-id", baseline, "--provider", config.provider, "--model", config.model,
        "--thinking-level", config.thinking, "--max-output-tokens", str(config.max_output_tokens or "unlimited"),
        "--outer-timeout-seconds", str(config.timeout_seconds), "--provider-routing-json", json.dumps(ROUTING_POLICY, sort_keys=True, separators=(",", ":")),
    ]
    for name, value in shell_environment.items():
        common.extend(["--shell-env", f"{name}={value}"])
    if config.pre_edit_tool_gate != "none":
        if baseline not in STATIC_BASELINES:
            raise ShootoutError("pre_edit_tool_gate may be forwarded only to static baselines")
        common.extend(["--pre-edit-tool-gate", config.pre_edit_tool_gate])
    if config.post_edit_validation_gate != "none":
        if baseline not in STATIC_BASELINES:
            raise ShootoutError("post_edit_validation_gate may be forwarded only to static baselines")
        common.extend(["--post-edit-validation-gate", config.post_edit_validation_gate])
    if baseline == "pi-static":
        command = ["node", str(SDK / "src" / "pi-adapter.ts"), *common]
    else:
        command = [
            str(ROOT / "target" / "debug" / "tea-eval"), *common,
            "--harness-mode", "jit" if baseline == "tea-jit" else "static",
        ]
        if config.tool_child_sandbox != "none":
            command.extend(["--tool-child-sandbox", config.tool_child_sandbox])
        if config.edit_recovery_projection != "none":
            command.extend(["--edit-recovery-projection", config.edit_recovery_projection])
        if config.static_prompt_profile != "builtin-v1":
            command.extend(["--static-prompt-profile", config.static_prompt_profile])
    return _vault_command(command)


def _attempt_id(repeat: int, baseline: str) -> str:
    return f"shootout-r{repeat + 1}-{baseline}"


def _runtime_revision() -> tuple[str, bool, str | None]:
    revision = subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True, capture_output=True, check=False).stdout.strip()
    dirty = bool(subprocess.run(["git", "status", "--porcelain=v1"], cwd=ROOT, text=True, capture_output=True, check=False).stdout)
    dirty_digest = None
    if dirty:
        diff = subprocess.run(["git", "diff", "--binary", "--no-ext-diff"], cwd=ROOT, text=True, capture_output=True, check=False).stdout
        dirty_digest = hashlib.sha256(diff.encode()).hexdigest()
    return revision, dirty, dirty_digest


def plan(config: Config) -> dict[str, Any]:
    config.validate()
    case = selected_case(config.task)
    baselines = ("tea-static",) if config.tea_only else (STATIC_BASELINES if config.static_only else BASELINES)
    condition_order = randomized_plan(config.repeats, config.seed, baselines)
    toolchain = toolchain_manifest()
    _, dependency_specification = validator_dependency_lockfile(case)
    return {
        "schema_version": "tea-pi-shootout-plan/v1",
        "task": config.task,
        "provider": config.provider,
        "model": config.model,
        "thinking": config.thinking,
        "max_output_tokens": config.max_output_tokens,
        "timeout_seconds": config.timeout_seconds,
        "provider_routing": ROUTING_POLICY,
        "validator_dependency_lockfile_sha256": dependency_specification["lockfile_sha256"],
        "run_class": "smoke-diagnostic" if config.repeats <= SMOKE_REPEATS else "serious-repeated-comparison",
        "toolchain_manifest": toolchain,
        "toolchain_manifest_sha256": toolchain["sha256"],
        "repeats": config.repeats,
        "parallel_repeats": config.effective_parallel_repeats(),
        "repeat_execution": "parallel lanes; sequential counterbalanced conditions within each lane",
        "seed": config.seed,
        "conditions": list(baselines),
        "static_only": config.static_only,
        "tea_only": config.tea_only,
        "tool_child_sandbox": config.tool_child_sandbox,
        "edit_recovery_projection": config.edit_recovery_projection,
        "pre_edit_tool_gate": config.pre_edit_tool_gate,
        "post_edit_validation_gate": config.post_edit_validation_gate,
        "source_local_targets": configured_source_local_targets(config, case),
        "static_prompt_profile": config.static_prompt_profile,
        "operator_stop_policy": "tea-only-diagnostic-v1" if config.tea_only else "disabled",
        "condition_order": condition_order,
        "baseline_commit": case["baseline"]["commit"],
        "known_correct_fix_commit": case["baseline"]["fix_commit"],
        "paths": {"cache_root": str(config.cache_root), "workspace_root": str(config.workspace_root), "out": str(config.out)},
        "credential_boundary": "vault OPENROUTER_API_KEY -- <adapter>",
    }


def _write_stop_target(config: Config, attempt_directory: Path, *, attempt_id: str, baseline: str, repeat: int) -> dict[str, Any]:
    eligible = config.tea_only and baseline == "tea-static"
    target = {
        "schema_version": STOP_TARGET_SCHEMA,
        "attempt_id": attempt_id,
        "baseline_id": baseline,
        "repeat_lane": repeat + 1,
        "operator_stop": {
            "eligible": eligible,
            "policy": "tea-only-diagnostic-v1" if eligible else "disabled",
        },
    }
    _write_json_once(attempt_directory / STOP_TARGET_FILENAME, target)
    return target


def _write_exclusion(
    attempt_directory: Path,
    *,
    attempt_id: str,
    baseline: str,
    repeat: int,
    process: ProcessOutcome,
    patch: str,
    changed_files: list[str],
) -> dict[str, Any]:
    """Persist an excluded lane without manufacturing an adapter result."""
    if process.operator_stop is None:
        raise ShootoutError("only a valid controller operator stop can produce an exclusion")
    exclusion = {
        "schema_version": EXCLUSION_SCHEMA,
        "kind": "operator_stopped",
        "attempt_id": attempt_id,
        "baseline_id": baseline,
        "repeat_lane": repeat,
        "stop_request": process.operator_stop,
        "process": {
            "pid": process.pid,
            "exit_code": process.exit_code,
            "timed_out": process.timed_out,
            "termination": {
                "signal": "SIGTERM",
                "escalated_to_sigkill": process.stop_escalated_to_kill,
                "observed_at": process.stop_observed_at,
            },
            "adapter_process_ms": process.elapsed_ms,
        },
        "adapter_command": ["vault", "OPENROUTER_API_KEY", "--", "<adapter redacted>"],
        "patch": {
            "sha256": hashlib.sha256(patch.encode()).hexdigest(),
            "changed_files": changed_files,
        },
        "excluded_at": _utc_timestamp(),
    }
    _write_json_once(attempt_directory / "exclusion.json", exclusion)
    return exclusion


def _split_attempt_outcomes(lanes: Iterable[Iterable[AttemptOutcome]]) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """Keep excluded lanes outside the benchmark attempt/result contract."""
    attempts: list[dict[str, Any]] = []
    exclusions: list[dict[str, Any]] = []
    for lane in lanes:
        for outcome in lane:
            if outcome.kind == "completed" and outcome.record is not None:
                attempts.append(outcome.record)
            elif outcome.kind == "excluded" and outcome.exclusion is not None:
                exclusions.append(outcome.exclusion)
            else:
                raise ShootoutError("invalid attempt outcome")
    return attempts, exclusions


def _attempt(
    config: Config,
    case: dict[str, Any],
    *,
    run_directory: Path,
    repeat: int,
    baseline: str,
    capabilities: list[dict[str, Any]],
    toolchain_manifest_sha256: str,
    cancellation: threading.Event,
) -> AttemptOutcome:
    _raise_if_cancelled(cancellation)
    attempt_directory = run_directory / "attempts" / (baseline if config.repeats == 1 else f"r{repeat + 1}-{baseline}")
    evidence = attempt_directory / "surface"
    evidence.mkdir(parents=True, exist_ok=False)
    attempt_id = _attempt_id(repeat, baseline)
    stop_target = _write_stop_target(
        config,
        attempt_directory,
        attempt_id=attempt_id,
        baseline=baseline,
        repeat=repeat,
    )
    worktree = materialize_clean_worktree(case, config.cache_root, config.workspace_root)
    started = time.monotonic_ns()
    try:
        _raise_if_cancelled(cancellation)
        assert_oracle_isolated_worktree(worktree.path, case["baseline"]["commit"], case["baseline"]["fix_commit"])
        repository_state = initial_workspace_state(worktree.path)
        home, temporary = attempt_directory / "home", attempt_directory / "tmp"
        home.mkdir()
        temporary.mkdir()
        try:
            _raise_if_cancelled(cancellation)
            dependency = provision_validator_dependencies(
                case,
                config.cache_root,
                attempt_directory / "validator-dependencies",
                populate_cache=False,
            )
        except CodingCaseError as error:
            raise ShootoutError(f"validator dependency setup failure: {error}") from error
        node_path = Path(dependency["node_path"])
        # The immutable cache prepared outside scoring is consumed only while
        # provisioning dependencies. Coding tools receive an empty, private
        # offline cache so concurrent model attempts cannot communicate through
        # npm metadata, logs, or a mutable cache entry.
        npm_cache = attempt_directory / "tool-npm-cache"
        npm_cache.mkdir()
        # Dependency installation is intentionally outside the Git workspace.
        # Confirm the checkout is still the exact clean baseline immediately
        # before the model receives it, then record that evidence with the run.
        workspace_state = initial_workspace_state(worktree.path)
        declared_source_local_targets = assert_source_local_targets_in_clean_workspace(case, worktree.path)
        _raise_if_cancelled(cancellation)
        shell = normalized_environment(home=home, temporary=temporary, npm_cache=npm_cache, node_path=node_path)
        curl_available = check_curl(shell, worktree.path)
        if not curl_available:
            raise ShootoutError("sanitized coding-tool environment cannot find curl")
        _raise_if_cancelled(cancellation)
        task = adapter_task(case, capabilities, config.timeout_seconds)
        task_path, capabilities_path, result_path = attempt_directory / "task.json", attempt_directory / "capabilities.json", attempt_directory / "adapter-result.json"
        task_path.write_bytes(canonical(task) + b"\n")
        capabilities_path.write_bytes(canonical(capabilities) + b"\n")
        command = adapter_command(config, baseline, task=task_path, workspace=worktree.path, capabilities=capabilities_path, result=result_path, evidence=evidence, attempt_id=attempt_id, shell_environment=shell)
        # The adapter is credentialed via vault. Its own coding-tool subprocesses receive only
        # the explicit shell environment sent in argv and never inherit OPENROUTER_API_KEY.
        # Vault itself may need the caller's ordinary home directory to locate
        # its non-provider credential store. That authority ends at the
        # adapter: both concrete coding-tool implementations use `shell`.
        adapter_environment = {"PATH": os.environ.get("PATH", ""), "LANG": "C", "LC_ALL": "C"}
        if os.environ.get("HOME"):
            adapter_environment["HOME"] = os.environ["HOME"]
        hard_timeout = attempt_hard_timeout_seconds(baseline, config.timeout_seconds)
        if config.tea_only:
            process = _run_process_with_stop(
                command,
                cwd=ROOT,
                environment=adapter_environment,
                timeout_seconds=hard_timeout,
                stop_target=stop_target,
                stop_request_path=attempt_directory / STOP_REQUEST_FILENAME,
                cancellation=cancellation,
            )
        else:
            process = _run_process(
                command,
                cwd=ROOT,
                environment=adapter_environment,
                timeout_seconds=hard_timeout,
                cancellation=cancellation,
            )
        (attempt_directory / "stdout.log").write_text(process.stdout, encoding="utf-8")
        (attempt_directory / "stderr.log").write_text(process.stderr, encoding="utf-8")
        _write_process_finalization(attempt_directory, process)
        if process.stop_protocol_error is not None:
            raise ShootoutError(f"{baseline} infrastructure failure: {process.stop_protocol_error}")
        _require_settled_process(baseline, process)
        if process.operator_stop is not None:
            patch = subprocess.run(["git", "diff", "--binary", "--no-ext-diff"], cwd=worktree.path, text=True, capture_output=True, check=False).stdout
            changed_files = subprocess.run(["git", "diff", "--name-only"], cwd=worktree.path, text=True, capture_output=True, check=False).stdout.splitlines()
            exclusion = _write_exclusion(
                attempt_directory,
                attempt_id=attempt_id,
                baseline=baseline,
                repeat=repeat + 1,
                process=process,
                patch=patch,
                changed_files=changed_files,
            )
            return AttemptOutcome.excluded(exclusion)
        code, timed_out, stdout, stderr, adapter_ms = (
            process.exit_code,
            process.timed_out,
            process.stdout,
            process.stderr,
            process.elapsed_ms,
        )
        result: dict[str, Any] | None = None
        contract_error: str | None = None
        try:
            result = validate_result(json.loads(result_path.read_text(encoding="utf-8")), attempt_id=attempt_id, baseline_id=baseline)
            if result["surface"]["shell_curl_available"] is not True:
                raise ContractError("adapter did not confirm shell curl availability")
            if result["surface"]["shell_environment_sha256"] != shell_environment_digest(
                shell,
                workspace=worktree.path,
                home=home,
                temporary=temporary,
                npm_cache=npm_cache,
                node_path=node_path,
            ):
                raise ContractError("adapter shell environment fingerprint disagrees with orchestrator")
        except (OSError, ValueError, ContractError) as error:
            contract_error = str(error)
        # An adapter may exit nonzero after publishing a valid terminal model
        # failure. That is benchmark data, not an infrastructure failure.
        if timed_out or contract_error is not None:
            raise ShootoutError(f"{baseline} infrastructure failure: timeout={timed_out}, exit={code}, result={contract_error or 'missing'}")
        _raise_if_cancelled(cancellation)
        validator_started = time.monotonic_ns()
        validator = run_validator(case, worktree.path, "fast", node_path=node_path)
        validator_ms = (time.monotonic_ns() - validator_started) // 1_000_000
        patch = subprocess.run(["git", "diff", "--binary", "--no-ext-diff"], cwd=worktree.path, text=True, capture_output=True, check=False).stdout
        (attempt_directory / "patch.diff").write_text(patch, encoding="utf-8")
        trace = result["trace"]
        (attempt_directory / "trace.jsonl").write_text("".join(json.dumps(item, sort_keys=True) + "\n" for item in trace), encoding="utf-8")
        validator_record = {"name": validator.name, "passed": validator.passed, "returncode": validator.returncode, "timed_out": validator.timed_out, "stdout": _bounded(validator.stdout), "stderr": _bounded(validator.stderr)}
        (attempt_directory / "validator.json").write_text(json.dumps(validator_record, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        record = {
            "baseline_id": baseline,
            "attempt_id": attempt_id,
            "repeat_lane": repeat + 1,
            "adapter_result": result,
            "adapter_command": ["vault", "OPENROUTER_API_KEY", "--", "<adapter redacted>"],
            "process": {"exit_code": code, "timed_out": timed_out, "peak_rss_bytes": None},
            "timings": {"adapter_process_ms": adapter_ms, "validator_ms": validator_ms, "total_attempt_ms": (time.monotonic_ns() - started) // 1_000_000},
            "validator": validator_record,
            "patch_sha256": hashlib.sha256(patch.encode()).hexdigest(),
            "initial_workspace_state": workspace_state,
            "repository_initial_workspace_state": repository_state,
            "validator_dependencies": {key: value for key, value in dependency.items() if key != "node_path"},
            "toolchain_manifest_sha256": toolchain_manifest_sha256,
            "attempt_isolation": {
                "workspace": "fresh detached baseline worktree",
                "validator_dependencies": "per-attempt node_modules outside workspace",
                "tool_npm_cache": "per-attempt offline cache",
                "tool_child_sandbox": config.tool_child_sandbox,
                "edit_recovery_projection": config.edit_recovery_projection,
                "pre_edit_tool_gate": config.pre_edit_tool_gate,
                "post_edit_validation_gate": config.post_edit_validation_gate,
                "source_local_targets": declared_source_local_targets if config.pre_edit_tool_gate == "source-local-v1" else [],
                "static_prompt_profile": config.static_prompt_profile,
            },
            "changed_files": subprocess.run(["git", "diff", "--name-only"], cwd=worktree.path, text=True, capture_output=True, check=False).stdout.splitlines(),
        }
        (attempt_directory / "record.json").write_text(json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        return AttemptOutcome.completed(record)
    finally:
        if not config.keep_worktrees:
            remove_worktree(worktree, config.workspace_root)


def run(
    config: Config,
    *,
    on_run_started: Callable[[Path], None] | None = None,
) -> tuple[Path, dict[str, Any]]:
    run_plan = plan(config)
    case = selected_case(config.task)
    capabilities = capability_manifest()
    cache_preparation = prepare_cache(cache_root=config.cache_root, case_ids=[config.task])
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    run_identity = digest({"plan": run_plan, "toolchain": run_plan["toolchain_manifest_sha256"]})
    run_id = f"{stamp}-{run_identity[:12]}"
    run_directory = config.out.resolve() / "runs" / run_id
    run_directory.mkdir(parents=True, exist_ok=False)
    # Establish the only shared directory before repeat lanes fan out. Each
    # lane then creates a unique child below this stable parent.
    (run_directory / "attempts").mkdir()
    revision, dirty, dirty_digest = _runtime_revision()
    manifest_path = Path(case["_manifest_path"])
    validator_path = manifest_path.parent / case["validators"]["fast"]["script"]
    run_metadata = {
        "run_id": run_id, "task_id": case["id"], "task_manifest_sha256": file_digest(manifest_path),
        "validator_sha256": file_digest(validator_path), "baseline_commit": case["baseline"]["commit"],
        "known_correct_fix_commit": case["baseline"]["fix_commit"], "provider": config.provider,
        "model": config.model, "thinking_level": config.thinking, "max_output_tokens": config.max_output_tokens, "timeout_seconds": config.timeout_seconds,
        "provider_routing": ROUTING_POLICY, "toolchain_manifest": run_plan["toolchain_manifest"], "toolchain_manifest_sha256": run_plan["toolchain_manifest_sha256"],
        "validator_dependency_lockfile_sha256": run_plan["validator_dependency_lockfile_sha256"],
        "validator_dependency_cache": cache_preparation["dependency_caches"].get(config.task),
        "run_class": run_plan["run_class"],
        "parallel_repeats": run_plan["parallel_repeats"],
        "condition_order": run_plan["condition_order"][0], "tea_revision": revision, "tea_dirty": dirty,
        "tea_dirty_digest": dirty_digest, "result_schema": RESULT_SCHEMA,
        "tool_child_sandbox": config.tool_child_sandbox,
        "edit_recovery_projection": config.edit_recovery_projection,
        "pre_edit_tool_gate": config.pre_edit_tool_gate,
        "post_edit_validation_gate": config.post_edit_validation_gate,
        "source_local_targets": configured_source_local_targets(config, case),
        "static_prompt_profile": config.static_prompt_profile,
        "operator_stop_policy": "tea-only-diagnostic-v1" if config.tea_only else "disabled",
    }
    (run_directory / "run.json").write_text(json.dumps({"plan": run_plan, "run": run_metadata}, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    if on_run_started is not None:
        on_run_started(run_directory)

    def run_repeat(repeat: int, order: list[str], cancellation: threading.Event) -> list[AttemptOutcome]:
        return [
            _attempt(
                config,
                case,
                run_directory=run_directory,
                repeat=repeat,
                baseline=baseline,
                capabilities=capabilities,
                toolchain_manifest_sha256=run_plan["toolchain_manifest_sha256"],
                cancellation=cancellation,
            )
            for baseline in order
        ]

    lanes = run_repeat_lanes(
        run_plan["condition_order"],
        run_plan["parallel_repeats"],
        run_repeat,
    )
    attempts, exclusions = _split_attempt_outcomes(lanes)
    summary = {
        "schema_version": "tea-pi-shootout-summary/v1",
        "run": run_metadata,
        "attempts": attempts,
        # Exclusions are evidence of an intentional Tea-only diagnostic stop,
        # never substitute adapter results, and are therefore outside every
        # comparison/report pairing input.
        "excluded_lanes": exclusions,
    }
    # Reports remain paired at every repeat: each static/evolution pair sees the
    # three attempts that shared one randomized order and fresh baseline copy.
    reports: tuple[Path, ...] = ()
    for repeat, order in enumerate(run_plan["condition_order"]):
        repeat_exclusions = [item for item in exclusions if item["repeat_lane"] == repeat + 1]
        if repeat_exclusions:
            # A partial single-baseline diagnostic cannot support a report,
            # and an excluded paired lane must never look like a valid pair.
            continue
        repeat_summary = {
            "schema_version": summary["schema_version"],
            "run": {**run_metadata, "condition_order": order, "repeat": repeat + 1},
            "attempts": [record for record in attempts if record["attempt_id"].startswith(f"shootout-r{repeat + 1}-")],
        }
        report_root = run_directory / "reports" if config.repeats == 1 else run_directory / "reports" / f"repeat-{repeat + 1}"
        if config.tea_only:
            reports += (write_baseline_report(repeat_summary, report_root, baseline="tea-static"),)
        elif config.static_only:
            reports += write_static_report(repeat_summary, report_root)
        else:
            reports += write_reports(repeat_summary, report_root)
    (run_directory / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return run_directory, {"summary": summary, "reports": [str(path) for path in reports]}
