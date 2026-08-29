"""Sequential, evidence-first runner for the pinned Pi/Tea shootout.

The module owns attempt placement and process/secret boundaries.  It reuses the
quality case cache, isolated worktree, and validator rather than creating a
second benchmark substrate.
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import random
import shutil
import signal
import subprocess
import time
from typing import Any, Iterable

from evals.quality.coding_cases import (
    CodingCaseError,
    assert_oracle_isolated_worktree,
    load_cases,
    materialize_clean_worktree,
    remove_worktree,
    run_validator,
)
from evals.quality.coding_runner import CodingRunError, coding_bundle_capabilities, prepare_cache

from .contract import BASELINES, ContractError, RESULT_SCHEMA, canonical, digest, file_digest, validate_result
from .report import write_reports


ROOT = Path(__file__).resolve().parents[2]
SDK = ROOT / "evals" / "pi_shootout" / "sdk"
DEFAULT_MODEL = "deepseek/deepseek-v4-flash-0731"
DEFAULT_THINKING = "high"
MAX_LOG_BYTES = 256 * 1024


class ShootoutError(RuntimeError):
    """A configuration or infrastructure boundary failed."""


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
    # condition and is excluded from agent-token accounting.
    timeout_seconds: int = 900
    keep_worktrees: bool = False

    def validate(self) -> None:
        if self.task != "express-3936-medium":
            raise ShootoutError("pi-shootout v0 supports only express-3936-medium")
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
        if not isinstance(self.timeout_seconds, int) or self.timeout_seconds < 1:
            raise ShootoutError("attempt timeout must be a positive integer")


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
    return {
        "schema_version": "tea-coding-eval-task/v1",
        "task_id": case["id"],
        "task_version": 1,
        "kind": "coding",
        "prompt": case["task"]["prompt"],
        "initial_workspace": [],
        "capabilities": capabilities,
        "timeout_seconds": timeout_seconds,
        "oracle_id": "quality-express-validator-v1",
    }


def randomized_plan(repeats: int, seed: int) -> list[list[str]]:
    if repeats < 1:
        raise ShootoutError("repeats must be positive")
    randomizer = random.Random(seed)
    return [randomizer.sample(list(BASELINES), len(BASELINES)) for _ in range(repeats)]


def normalized_environment(*, home: Path, temporary: Path, npm_cache: Path | None = None) -> dict[str, str]:
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
    return environment


def _replace_attempt_paths(value: str, *, workspace: Path, home: Path, temporary: Path, npm_cache: Path | None) -> str:
    replacements = [(str(workspace), "{WORKSPACE}"), (str(home), "{HOME}"), (str(temporary), "{TMPDIR}")]
    if npm_cache is not None:
        replacements.append((str(npm_cache), "{NPM_CACHE}"))
    for source, target in replacements:
        value = value.replace(source, target)
    return value


def shell_environment_digest(environment: dict[str, str], *, workspace: Path, home: Path, temporary: Path, npm_cache: Path | None) -> str:
    public = {
        name: _replace_attempt_paths(value, workspace=workspace, home=home, temporary=temporary, npm_cache=npm_cache)
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


def _run_process(command: list[str], *, cwd: Path, environment: dict[str, str], timeout_seconds: int) -> tuple[int | None, bool, str, str, int]:
    started = time.monotonic_ns()
    process = subprocess.Popen(command, cwd=cwd, env=environment, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=os.name == "posix")
    try:
        stdout, stderr = process.communicate(timeout=timeout_seconds)
        return process.returncode, False, _bounded(stdout), _bounded(stderr), (time.monotonic_ns() - started) // 1_000_000
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
        return None, True, _bounded(stdout), _bounded(stderr), (time.monotonic_ns() - started) // 1_000_000


def _vault_command(command: list[str]) -> list[str]:
    """Keep key injection at the caller-visible final live child boundary."""
    return ["vault", "OPENROUTER_API_KEY", "--", *command]


def adapter_command(config: Config, baseline: str, *, task: Path, workspace: Path, capabilities: Path, result: Path, evidence: Path, attempt_id: str, shell_environment: dict[str, str]) -> list[str]:
    common = [
        "--task-json", str(task), "--workspace", str(workspace), "--capabilities-json", str(capabilities),
        "--result-json", str(result), "--evidence-dir", str(evidence), "--attempt-id", attempt_id,
        "--baseline-id", baseline, "--provider", config.provider, "--model", config.model,
        "--thinking-level", config.thinking, "--max-output-tokens", str(config.max_output_tokens or "unlimited"),
    ]
    for name, value in shell_environment.items():
        common.extend(["--shell-env", f"{name}={value}"])
    if baseline == "pi-static":
        command = ["node", str(SDK / "src" / "pi-adapter.ts"), *common]
    else:
        command = [
            str(ROOT / "target" / "debug" / "tea-eval"), *common,
            "--harness-mode", "jit" if baseline == "tea-jit" else "static",
        ]
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
    condition_order = randomized_plan(config.repeats, config.seed)
    return {
        "schema_version": "tea-pi-shootout-plan/v1",
        "task": config.task,
        "provider": config.provider,
        "model": config.model,
        "thinking": config.thinking,
        "max_output_tokens": config.max_output_tokens,
        "timeout_seconds": config.timeout_seconds,
        "repeats": config.repeats,
        "seed": config.seed,
        "conditions": list(BASELINES),
        "condition_order": condition_order,
        "baseline_commit": case["baseline"]["commit"],
        "known_correct_fix_commit": case["baseline"]["fix_commit"],
        "paths": {"cache_root": str(config.cache_root), "workspace_root": str(config.workspace_root), "out": str(config.out)},
        "credential_boundary": "vault OPENROUTER_API_KEY -- <adapter>",
    }


def _attempt(config: Config, case: dict[str, Any], *, run_directory: Path, repeat: int, baseline: str, capabilities: list[dict[str, Any]]) -> dict[str, Any]:
    attempt_directory = run_directory / "attempts" / (baseline if config.repeats == 1 else f"r{repeat + 1}-{baseline}")
    evidence = attempt_directory / "surface"
    evidence.mkdir(parents=True, exist_ok=False)
    worktree = materialize_clean_worktree(case, config.cache_root, config.workspace_root)
    started = time.monotonic_ns()
    try:
        assert_oracle_isolated_worktree(worktree.path, case["baseline"]["commit"], case["baseline"]["fix_commit"])
        home, temporary = attempt_directory / "home", attempt_directory / "tmp"
        home.mkdir()
        temporary.mkdir()
        shell = normalized_environment(home=home, temporary=temporary)
        curl_available = check_curl(shell, worktree.path)
        if not curl_available:
            raise ShootoutError("sanitized coding-tool environment cannot find curl")
        task = adapter_task(case, capabilities, config.timeout_seconds)
        task_path, capabilities_path, result_path = attempt_directory / "task.json", attempt_directory / "capabilities.json", attempt_directory / "adapter-result.json"
        task_path.write_bytes(canonical(task) + b"\n")
        capabilities_path.write_bytes(canonical(capabilities) + b"\n")
        attempt_id = _attempt_id(repeat, baseline)
        command = adapter_command(config, baseline, task=task_path, workspace=worktree.path, capabilities=capabilities_path, result=result_path, evidence=evidence, attempt_id=attempt_id, shell_environment=shell)
        # The adapter is credentialed via vault. Its own coding-tool subprocesses receive only
        # the explicit shell environment sent in argv and never inherit OPENROUTER_API_KEY.
        # Vault itself may need the caller's ordinary home directory to locate
        # its non-provider credential store. That authority ends at the
        # adapter: both concrete coding-tool implementations use `shell`.
        adapter_environment = {"PATH": os.environ.get("PATH", ""), "LANG": "C", "LC_ALL": "C"}
        if os.environ.get("HOME"):
            adapter_environment["HOME"] = os.environ["HOME"]
        code, timed_out, stdout, stderr, adapter_ms = _run_process(command, cwd=ROOT, environment=adapter_environment, timeout_seconds=config.timeout_seconds)
        (attempt_directory / "stdout.log").write_text(stdout, encoding="utf-8")
        (attempt_directory / "stderr.log").write_text(stderr, encoding="utf-8")
        result: dict[str, Any] | None = None
        contract_error: str | None = None
        try:
            result = validate_result(json.loads(result_path.read_text(encoding="utf-8")), attempt_id=attempt_id, baseline_id=baseline)
            if result["surface"]["shell_curl_available"] is not True:
                raise ContractError("adapter did not confirm shell curl availability")
            if result["surface"]["shell_environment_sha256"] != shell_environment_digest(shell, workspace=worktree.path, home=home, temporary=temporary, npm_cache=None):
                raise ContractError("adapter shell environment fingerprint disagrees with orchestrator")
        except (OSError, ValueError, ContractError) as error:
            contract_error = str(error)
        # An adapter may exit nonzero after publishing a valid terminal model
        # failure. That is benchmark data, not an infrastructure failure.
        if timed_out or contract_error is not None:
            raise ShootoutError(f"{baseline} infrastructure failure: timeout={timed_out}, exit={code}, result={contract_error or 'missing'}")
        validator_started = time.monotonic_ns()
        validator = run_validator(case, worktree.path, "fast")
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
            "adapter_result": result,
            "adapter_command": ["vault", "OPENROUTER_API_KEY", "--", "<adapter redacted>"],
            "process": {"exit_code": code, "timed_out": timed_out, "peak_rss_bytes": None},
            "timings": {"adapter_process_ms": adapter_ms, "validator_ms": validator_ms, "total_attempt_ms": (time.monotonic_ns() - started) // 1_000_000},
            "validator": validator_record,
            "patch_sha256": hashlib.sha256(patch.encode()).hexdigest(),
            "changed_files": subprocess.run(["git", "diff", "--name-only"], cwd=worktree.path, text=True, capture_output=True, check=False).stdout.splitlines(),
        }
        (attempt_directory / "record.json").write_text(json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        return record
    finally:
        if not config.keep_worktrees:
            remove_worktree(worktree, config.workspace_root)


def run(config: Config) -> tuple[Path, dict[str, Any]]:
    run_plan = plan(config)
    case = selected_case(config.task)
    capabilities = capability_manifest()
    prepare_cache(cache_root=config.cache_root, case_ids=[config.task])
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    run_id = f"{stamp}-{digest(run_plan)[:12]}"
    run_directory = config.out.resolve() / "runs" / run_id
    run_directory.mkdir(parents=True, exist_ok=False)
    revision, dirty, dirty_digest = _runtime_revision()
    manifest_path = Path(case["_manifest_path"])
    validator_path = manifest_path.parent / case["validators"]["fast"]["script"]
    run_metadata = {
        "run_id": run_id, "task_id": case["id"], "task_manifest_sha256": file_digest(manifest_path),
        "validator_sha256": file_digest(validator_path), "baseline_commit": case["baseline"]["commit"],
        "known_correct_fix_commit": case["baseline"]["fix_commit"], "provider": config.provider,
        "model": config.model, "thinking_level": config.thinking, "max_output_tokens": config.max_output_tokens, "timeout_seconds": config.timeout_seconds,
        "condition_order": run_plan["condition_order"][0], "tea_revision": revision, "tea_dirty": dirty,
        "tea_dirty_digest": dirty_digest, "result_schema": RESULT_SCHEMA,
    }
    (run_directory / "run.json").write_text(json.dumps({"plan": run_plan, "run": run_metadata}, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    attempts = []
    for repeat, order in enumerate(run_plan["condition_order"]):
        for baseline in order:
            attempts.append(_attempt(config, case, run_directory=run_directory, repeat=repeat, baseline=baseline, capabilities=capabilities))
    summary = {"schema_version": "tea-pi-shootout-summary/v1", "run": run_metadata, "attempts": attempts}
    # Reports remain paired at every repeat: each static/evolution pair sees the
    # three attempts that shared one randomized order and fresh baseline copy.
    reports: tuple[Path, ...] = ()
    for repeat, order in enumerate(run_plan["condition_order"]):
        repeat_summary = {
            "schema_version": summary["schema_version"],
            "run": {**run_metadata, "condition_order": order, "repeat": repeat + 1},
            "attempts": [record for record in attempts if record["attempt_id"].startswith(f"shootout-r{repeat + 1}-")],
        }
        report_root = run_directory / "reports" if config.repeats == 1 else run_directory / "reports" / f"repeat-{repeat + 1}"
        reports += write_reports(repeat_summary, report_root)
    (run_directory / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return run_directory, {"summary": summary, "reports": [str(path) for path in reports]}
