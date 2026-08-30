"""Sequential, evidence-first runner for the pinned Pi/Tea shootout.

The module owns attempt placement and process/secret boundaries.  It reuses the
quality case cache, isolated worktree, and validator rather than creating a
second benchmark substrate.
"""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor, as_completed
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
    run_repeat: Callable[[int, list[str]], RepeatResult],
) -> list[RepeatResult]:
    """Run independent repeats concurrently while preserving per-repeat order.

    ``run_repeat`` owns every workspace, evidence directory, dependency tree,
    and child process for its lane. Results are returned in repeat order rather
    than completion order, making persisted artifacts deterministic even when
    provider latency differs across the parallel lanes.
    """
    if not orders or parallel_repeats < 1 or parallel_repeats > len(orders):
        raise ShootoutError("parallel repeat lane count must be between one and the number of repeats")
    if parallel_repeats == 1:
        return [run_repeat(repeat, order) for repeat, order in enumerate(orders)]
    results: dict[int, RepeatResult] = {}
    with ThreadPoolExecutor(max_workers=parallel_repeats, thread_name_prefix="tea-shootout-repeat") as executor:
        pending = {
            executor.submit(run_repeat, repeat, order): repeat
            for repeat, order in enumerate(orders)
        }
        for future in as_completed(pending):
            results[pending[future]] = future.result()
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


def _run_process(command: list[str], *, cwd: Path, environment: dict[str, str], timeout_seconds: int) -> tuple[int | None, bool, str, str, int]:
    started = time.monotonic_ns()
    process = subprocess.Popen(command, cwd=cwd, env=environment, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=os.name == "posix")
    try:
        if timeout_seconds == 0:
            stdout, stderr = process.communicate()
        else:
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
        "--outer-timeout-seconds", str(config.timeout_seconds), "--provider-routing-json", json.dumps(ROUTING_POLICY, sort_keys=True, separators=(",", ":")),
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
        "condition_order": condition_order,
        "baseline_commit": case["baseline"]["commit"],
        "known_correct_fix_commit": case["baseline"]["fix_commit"],
        "paths": {"cache_root": str(config.cache_root), "workspace_root": str(config.workspace_root), "out": str(config.out)},
        "credential_boundary": "vault OPENROUTER_API_KEY -- <adapter>",
    }


def _attempt(
    config: Config,
    case: dict[str, Any],
    *,
    run_directory: Path,
    repeat: int,
    baseline: str,
    capabilities: list[dict[str, Any]],
    toolchain_manifest_sha256: str,
) -> dict[str, Any]:
    attempt_directory = run_directory / "attempts" / (baseline if config.repeats == 1 else f"r{repeat + 1}-{baseline}")
    evidence = attempt_directory / "surface"
    evidence.mkdir(parents=True, exist_ok=False)
    worktree = materialize_clean_worktree(case, config.cache_root, config.workspace_root)
    started = time.monotonic_ns()
    try:
        assert_oracle_isolated_worktree(worktree.path, case["baseline"]["commit"], case["baseline"]["fix_commit"])
        repository_state = initial_workspace_state(worktree.path)
        home, temporary = attempt_directory / "home", attempt_directory / "tmp"
        home.mkdir()
        temporary.mkdir()
        try:
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
        shell = normalized_environment(home=home, temporary=temporary, npm_cache=npm_cache, node_path=node_path)
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
            },
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
    }
    (run_directory / "run.json").write_text(json.dumps({"plan": run_plan, "run": run_metadata}, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    def run_repeat(repeat: int, order: list[str]) -> list[dict[str, Any]]:
        return [
            _attempt(
                config,
                case,
                run_directory=run_directory,
                repeat=repeat,
                baseline=baseline,
                capabilities=capabilities,
                toolchain_manifest_sha256=run_plan["toolchain_manifest_sha256"],
            )
            for baseline in order
        ]

    attempts = [
        attempt
        for lane in run_repeat_lanes(
            run_plan["condition_order"],
            run_plan["parallel_repeats"],
            run_repeat,
        )
        for attempt in lane
    ]
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
        if config.tea_only:
            reports += (write_baseline_report(repeat_summary, report_root, baseline="tea-static"),)
        elif config.static_only:
            reports += write_static_report(repeat_summary, report_root)
        else:
            reports += write_reports(repeat_summary, report_root)
    (run_directory / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return run_directory, {"summary": summary, "reports": [str(path) for path in reports]}
