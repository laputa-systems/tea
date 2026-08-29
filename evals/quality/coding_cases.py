"""Hermetic setup and validation helpers for the native coding quality cases.

The coding cases intentionally keep the benchmark's repository and dependency concerns
outside the model adapters.  A case is materialized by copying objects from a locked bare
repository cache into a fresh detached clone; the cache is never used as a worktree.  This
means an agent cannot contaminate a later attempt, and a validator always observes the exact
commit named by the case.

This module is stdlib-only and does not replace the older ``evals.controller`` contract.
"""

from __future__ import annotations

from contextlib import contextmanager
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
from typing import Any, Iterator


ROOT = Path(__file__).resolve().parent
CASES = ROOT / "cases" / "coding"
FORMAT_VERSION = 1
CASE_KIND = "quality_coding_case"
PI_BENCH_COMMIT = "8fbd7c6015a1ebaf1fd1d2bf257d066106aa3bb5"
PI_BENCH_REPOSITORY = "https://github.com/kyuz0/pi-bench.git"
EXPRESS_REPOSITORY = "https://github.com/expressjs/express.git"
EXPECTED = {
    "express-4744-easy": {
        "commit": "9dd0e7afdb6d022e18add1e009c4e3a66258c1fa",
        "fix_commit": "f275e87dff1aaef86080e6931888de4968585fd8",
        "source_path": "tasks/curated/easy.json",
    },
    "express-3936-medium": {
        "commit": "1cc816993832eba829a2f556f7c08e27e6371301",
        "fix_commit": "5855339455a7f60774bef4166829e742a5056fa8",
        "source_path": "tasks/curated/medium.json",
    },
    "express-4205-hard": {
        "commit": "bdc2c973a468d83f3af7d57d862f74ca97e71322",
        "fix_commit": "99a369f3d51bafcf0c09657250067249f19d04f5",
        "source_path": "tasks/curated/hard.json",
    },
}
SHA1 = re.compile(r"^[0-9a-f]{40}$")
SAFE_ID = re.compile(r"^[a-z0-9][a-z0-9-]{0,63}$")


class CodingCaseError(ValueError):
    """A coding case, cache, worktree, or validator contract is unsafe or invalid."""


def _string(value: Any, name: str) -> str:
    if not isinstance(value, str) or not value:
        raise CodingCaseError(f"{name} must be a non-empty string")
    return value


def _commit(value: Any, name: str) -> str:
    value = _string(value, name)
    if not SHA1.fullmatch(value):
        raise CodingCaseError(f"{name} must be a full lowercase commit hash")
    return value


def _argv(value: Any, name: str) -> list[str]:
    if not isinstance(value, list) or not value or any(
        not isinstance(part, str) or not part or "\x00" in part for part in value
    ):
        raise CodingCaseError(f"{name} must be a non-empty argv array")
    if any(part in {"&&", ";", "|", "||"} for part in value):
        raise CodingCaseError(f"{name} must not contain shell operators")
    return value


def _relative(value: Any, name: str) -> Path:
    value = _string(value, name)
    path = Path(value)
    if path.is_absolute() or "\\" in value or any(part in ("", ".", "..") for part in path.parts):
        raise CodingCaseError(f"{name} must be a safe relative path")
    return path


def _read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise CodingCaseError(f"cannot read {path}: {exc}") from exc
    if not isinstance(value, dict):
        raise CodingCaseError(f"{path} must contain an object")
    return value


def validate_case(case: dict[str, Any], source: str = "case") -> dict[str, Any]:
    if case.get("format_version") != FORMAT_VERSION or case.get("kind") != CASE_KIND:
        raise CodingCaseError(f"{source}: unsupported coding case format")
    case_id = _string(case.get("id"), f"{source}.id")
    if not SAFE_ID.fullmatch(case_id) or case_id not in EXPECTED:
        raise CodingCaseError(f"{source}: unexpected coding case id {case_id!r}")
    expected = EXPECTED[case_id]
    if case.get("scope") != "coding":
        raise CodingCaseError(f"{source}: scope must be coding")
    _string(case.get("description"), f"{source}.description")
    source_info = case.get("source")
    if not isinstance(source_info, dict):
        raise CodingCaseError(f"{source}: source must be an object")
    if source_info.get("repository") != PI_BENCH_REPOSITORY:
        raise CodingCaseError(f"{source}: source repository is not pinned pi-bench")
    if source_info.get("commit") != PI_BENCH_COMMIT:
        raise CodingCaseError(f"{source}: source commit is not the checked-in pi-bench pin")
    if source_info.get("path") != expected["source_path"]:
        raise CodingCaseError(f"{source}: source path does not match the curated task")
    if source_info.get("task_id") != case_id:
        raise CodingCaseError(f"{source}: source task_id does not match id")
    repository = case.get("baseline")
    if not isinstance(repository, dict):
        raise CodingCaseError(f"{source}: baseline must be an object")
    if repository.get("repository") != EXPRESS_REPOSITORY:
        raise CodingCaseError(f"{source}: baseline repository is not pinned Express")
    if repository.get("commit") != expected["commit"]:
        raise CodingCaseError(f"{source}: baseline commit does not match requested pin")
    _commit(repository.get("fix_commit"), f"{source}.baseline.fix_commit")
    if repository["fix_commit"] != expected["fix_commit"]:
        raise CodingCaseError(f"{source}: known-correct fix commit is not the issue fix")
    task = case.get("task")
    if not isinstance(task, dict):
        raise CodingCaseError(f"{source}: task must be an object")
    _string(task.get("prompt"), f"{source}.task.prompt")
    _string(task.get("expected_behavior"), f"{source}.task.expected_behavior")
    setup = case.get("setup")
    if not isinstance(setup, dict) or setup.get("clean_worktree") is not True:
        raise CodingCaseError(f"{source}: setup.clean_worktree must be true")
    if setup.get("network") is not False:
        raise CodingCaseError(f"{source}: setup.network must be false")
    if setup.get("tools") != ["read", "bash", "edit", "find"]:
        raise CodingCaseError(f"{source}: coding tool surface must be pinned")
    validators = case.get("validators")
    if not isinstance(validators, dict) or not isinstance(validators.get("full"), dict):
        raise CodingCaseError(f"{source}: full audit validator is required")
    full = validators["full"]
    if full.get("audit_command") != "npm install && npm test":
        raise CodingCaseError(f"{source}: full validator must preserve pi-bench audit command")
    commands = full.get("commands")
    if not isinstance(commands, list) or len(commands) != 2:
        raise CodingCaseError(f"{source}: full validator needs install and test commands")
    if _argv(commands[0], f"{source}.validators.full.commands[0]")[:2] != ["npm", "install"]:
        raise CodingCaseError(f"{source}: full validator must install dependencies first")
    if _argv(commands[1], f"{source}.validators.full.commands[1]")[:2] != ["npm", "test"]:
        raise CodingCaseError(f"{source}: full validator must run npm test")
    fast = validators.get("fast")
    if not isinstance(fast, dict):
        raise CodingCaseError(f"{source}: fast validator must include baseline/fix evidence")
    script = _relative(fast.get("script"), f"{source}.validators.fast.script")
    manifest_hint = Path(case.get("_manifest_path", source))
    if not (manifest_hint.parent / script).is_file():
        raise CodingCaseError(f"{source}: fast validator script is missing")
    _argv(fast.get("command"), f"{source}.validators.fast.command")
    evidence = fast.get("evidence")
    if not isinstance(evidence, dict) or evidence.get("baseline") != "fails" or evidence.get("known_correct") != "passes":
        raise CodingCaseError(f"{source}: fast validator lacks baseline/fix evidence")
    if evidence.get("direct_regression") is not True:
        raise CodingCaseError(f"{source}: fast validator must test the bug directly")
    if evidence.get("baseline_exit_code") != 1 or evidence.get("known_correct_exit_code") != 0:
        raise CodingCaseError(f"{source}: fast validator evidence must record fail/pass exit codes")
    if not isinstance(fast.get("timeout_seconds"), int) or fast["timeout_seconds"] < 1:
        raise CodingCaseError(f"{source}: fast validator timeout must be positive")
    return case


def load_case(path: Path) -> dict[str, Any]:
    case = validate_case(_read_json(path), str(path))
    # This private field only resolves a read-only validator script; it is not part
    # of the public manifest and is ignored by all contract fields above.
    return attach_manifest_path(case, path)


def load_cases(root: Path = CASES) -> list[dict[str, Any]]:
    files = sorted(root.glob("*/manifest.json"))
    if {path.parent.name for path in files} != set(EXPECTED):
        raise CodingCaseError("coding suite must contain exactly the three requested cases")
    cases = [load_case(path) for path in files]
    if [case["id"] for case in cases] != sorted(EXPECTED):
        raise CodingCaseError("coding case ids must be unique")
    return cases


def _explicit_root(path: Path, name: str) -> Path:
    resolved = path.expanduser().resolve()
    if resolved == Path(resolved.anchor) or resolved == Path.home().resolve():
        raise CodingCaseError(f"{name} must be an explicit child directory")
    resolved.mkdir(parents=True, exist_ok=True)
    return resolved


@contextmanager
def _cache_lock(path: Path) -> Iterator[None]:
    """Serialize cache population without adding a locking dependency."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a+") as lock:
        try:
            import fcntl
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
        except (ImportError, OSError):
            # Windows has no fcntl; atomic cache publication still prevents partial clones.
            pass
        try:
            yield
        finally:
            try:
                import fcntl
                fcntl.flock(lock.fileno(), fcntl.LOCK_UN)
            except (ImportError, OSError):
                pass


def _git(*args: str, cwd: Path | None = None, timeout: int = 300) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(["git", *args], cwd=cwd, text=True, capture_output=True, timeout=timeout, check=True)
    except (OSError, subprocess.SubprocessError) as exc:
        raise CodingCaseError(f"git command failed: git {' '.join(args)}: {exc}") from exc


def cache_bare_repository(repository: str, commit: str, cache_root: Path, *, populate: bool = False) -> Path:
    """Return a bare cache containing ``commit``, publishing it atomically.

    A scoring run is offline with respect to repository setup.  Cold-cache
    population is a separate, explicit maintenance action so a model attempt
    cannot silently gain network access before its adapter starts.
    """
    if repository != EXPRESS_REPOSITORY:
        raise CodingCaseError("only the pinned Express repository is allowed")
    _commit(commit, "commit")
    root = _explicit_root(cache_root, "cache_root") / "bare"
    root.mkdir(parents=True, exist_ok=True)
    key = hashlib.sha256(repository.encode()).hexdigest()[:32]
    bare = root / f"{key}.git"
    # A clone only advertises normal branch/tag refs, not arbitrary loose/unreachable objects.
    # Keep a cache-private branch after verifying each allowed object so a detached
    # materialization can reliably obtain both independently shallow-pinned commits.
    cache_ref = f"refs/heads/tea-quality/{commit}"
    with _cache_lock(root / f"{key}.lock"):
        if bare.is_symlink():
            raise CodingCaseError("bare repository cache entry must not be a symlink")
        if not bare.exists():
            if not populate:
                raise CodingCaseError(
                    f"missing pinned bare repository cache {bare}; run the explicit prepare-cache command first"
                )
            temporary = Path(tempfile.mkdtemp(prefix=f".{key}.", dir=root))
            try:
                _git("init", "--bare", str(temporary))
                _git("--git-dir", str(temporary), "remote", "add", "origin", repository)
                _git("--git-dir", str(temporary), "fetch", "--depth=1", "origin", commit, timeout=600)
                os.replace(temporary, bare)
            except BaseException:
                shutil.rmtree(temporary, ignore_errors=True)
                raise
        try:
            _git("--git-dir", str(bare), "cat-file", "-e", f"{commit}^{{commit}}")
        except CodingCaseError:
            if not populate:
                raise CodingCaseError(
                    f"pinned commit {commit} is absent from bare cache {bare}; run prepare-cache first"
                ) from None
            _git("--git-dir", str(bare), "fetch", "--depth=1", "origin", commit, timeout=600)
            _git("--git-dir", str(bare), "cat-file", "-e", f"{commit}^{{commit}}")
        _git("--git-dir", str(bare), "update-ref", cache_ref, commit)
    return bare


def dependency_cache_path(workspace: Path, cache_root: Path) -> Path:
    """Return the npm content cache keyed by the checked-out lockfile.

    Only npm's content-addressed download cache is shared.  ``node_modules`` is never
    shared between attempts, so install scripts or a changed dependency tree cannot
    contaminate a later clean worktree.
    """
    lockfile = workspace / "package-lock.json"
    if not lockfile.is_file() or lockfile.is_symlink():
        raise CodingCaseError("dependency caching requires a regular package-lock.json")
    key = hashlib.sha256(lockfile.read_bytes()).hexdigest()
    root = _explicit_root(cache_root, "cache_root") / "npm" / key
    root.mkdir(parents=True, exist_ok=True)
    return root


@dataclass(frozen=True)
class CleanWorktree:
    path: Path
    commit: str
    bare_repository: Path


def materialize_clean_worktree(
    case: dict[str, Any], cache_root: Path, workspace_root: Path, *, populate_cache: bool = False
) -> CleanWorktree:
    """Materialize only the baseline object into a detached attempt repository.

    The cache deliberately retains the known-correct fix so the validator contract
    can be audited.  An attempt must not inherit that object merely because it
    shares the cache: models may inspect their local object database through the
    normal ``bash`` tool.  Fetching the one shallow baseline object into a fresh
    repository keeps the attempt useful as a Git checkout while making the oracle
    commit unavailable to the model.
    """
    validate_case(case)
    commit = case["baseline"]["commit"]
    bare = cache_bare_repository(case["baseline"]["repository"], commit, cache_root, populate=populate_cache)
    parent = _explicit_root(workspace_root, "workspace_root")
    path = Path(tempfile.mkdtemp(prefix=f"{case['id']}-", dir=parent))
    try:
        # Do not clone the cache. A clone copies every reachable cache ref,
        # including the validator's known-good oracle. An explicit shallow fetch
        # is the narrow object transfer boundary for an agent workspace.
        _git("init", str(path), timeout=600)
        _git(
            "-C",
            str(path),
            "-c",
            "protocol.file.allow=always",
            "fetch",
            "--depth=1",
            str(bare),
            commit,
            timeout=600,
        )
        _git("checkout", "--detach", "--force", "FETCH_HEAD", cwd=path)
        _git("remote", "remove", "origin", cwd=path) if _git("remote", cwd=path).stdout.strip() else None
        _git("reflog", "expire", "--expire=now", "--all", cwd=path)
        _git("gc", "--prune=now", cwd=path)
        actual = _git("rev-parse", "HEAD", cwd=path).stdout.strip()
        status = _git("status", "--porcelain=v1", "--untracked-files=all", cwd=path).stdout
        if actual != commit or status:
            raise CodingCaseError("materialized worktree is not the requested clean commit")
        assert_oracle_isolated_worktree(path, commit, case["baseline"]["fix_commit"])
    except BaseException:
        shutil.rmtree(path, ignore_errors=True)
        raise
    return CleanWorktree(path, commit, bare)


def assert_oracle_isolated_worktree(workspace: Path, baseline_commit: str, fix_commit: str) -> None:
    """Verify that an attempt contains its baseline but not the audited fix object."""
    _commit(baseline_commit, "baseline_commit")
    _commit(fix_commit, "fix_commit")
    actual = _git("rev-parse", "HEAD", cwd=workspace).stdout.strip()
    status = _git("status", "--porcelain=v1", "--untracked-files=all", cwd=workspace).stdout
    remotes = _git("remote", cwd=workspace).stdout.strip()
    probe = subprocess.run(
        ["git", "cat-file", "-e", f"{fix_commit}^{{commit}}"],
        cwd=workspace,
        text=True,
        capture_output=True,
        check=False,
    )
    if actual != baseline_commit or status or remotes or probe.returncode == 0:
        raise CodingCaseError("attempt worktree exposes a non-baseline Git oracle")


def remove_worktree(worktree: CleanWorktree, workspace_root: Path) -> None:
    """Remove only a worktree previously created below the explicit workspace root."""
    root = _explicit_root(workspace_root, "workspace_root")
    raw_path = worktree.path
    if raw_path.is_symlink():
        raise CodingCaseError("refusing to remove a symlink worktree")
    path = raw_path.resolve()
    try:
        path.relative_to(root)
    except ValueError as exc:
        raise CodingCaseError("refusing to remove a worktree outside workspace_root") from exc
    if path == root or path.is_symlink():
        raise CodingCaseError("refusing to remove an unsafe worktree path")
    shutil.rmtree(path)


@dataclass(frozen=True)
class ValidatorResult:
    name: str
    passed: bool
    returncode: int | None
    timed_out: bool
    stdout: str
    stderr: str


def _safe_environment() -> dict[str, str]:
    return {key: value for key, value in {
        "PATH": os.environ.get("PATH", ""), "LANG": "C", "LC_ALL": "C",
        "NPM_CONFIG_AUDIT": "false", "NPM_CONFIG_FUND": "false",
    }.items() if value}


def run_validator(
    case: dict[str, Any], workspace: Path, name: str = "fast", *, dependency_cache: Path | None = None
) -> ValidatorResult:
    """Run a structured validator with no shell and no ambient credentials."""
    validate_case(case)
    workspace = workspace.resolve()
    if not workspace.is_dir() or workspace.is_symlink():
        raise CodingCaseError("validator workspace must be a real directory")
    validator = case["validators"].get(name)
    if not isinstance(validator, dict):
        raise CodingCaseError(f"unknown validator {name!r}")
    commands: list[list[str]]
    if name == "full":
        commands = validator["commands"]
    else:
        command = list(validator["command"])
        script = (Path(case["_manifest_path"]).parent / validator["script"]).resolve() if case.get("_manifest_path") else None
        if script is None or not script.is_file() or script.is_symlink():
            raise CodingCaseError("fast validator script must be a regular file")
        replacements = {"{validator_script}": str(script), "{workspace}": str(workspace)}
        commands = [[part.replace("{validator_script}", replacements["{validator_script}"]).replace("{workspace}", str(workspace)) for part in command]]
    environment = _safe_environment()
    if dependency_cache is not None:
        cache = dependency_cache.resolve()
        if not cache.is_dir() or cache.is_symlink():
            raise CodingCaseError("dependency_cache must be a real directory")
        environment["npm_config_cache"] = str(cache)
    output: list[str] = []
    errors: list[str] = []
    for command in commands:
        _argv(command, f"{name} validator command")
        if any("{" in part or "}" in part for part in command):
            raise CodingCaseError("validator command contains an unresolved placeholder")
        try:
            completed = subprocess.run(command, cwd=workspace, env=environment, text=True,
                                       capture_output=True, timeout=validator.get("timeout_seconds", 900), check=False)
            output.append(completed.stdout)
            errors.append(completed.stderr)
        except subprocess.TimeoutExpired as exc:
            return ValidatorResult(name, False, None, True, "".join(output), "".join(errors) + str(exc))
        if completed.returncode != 0:
            return ValidatorResult(name, False, completed.returncode, False, "".join(output), "".join(errors))
    return ValidatorResult(name, True, 0, False, "".join(output), "".join(errors))


def attach_manifest_path(case: dict[str, Any], manifest_path: Path) -> dict[str, Any]:
    """Attach an internal path used only to resolve the read-only fast test script."""
    result = dict(case)
    result["_manifest_path"] = str(manifest_path.resolve())
    return result
