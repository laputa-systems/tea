"""Provider-free coding-case metadata and explicit source-cache preparation.

The historical arbitrary-model runner was removed. Live inference is allowed
only through the feature-gated Rust restricted-factory example, never from
this Python module.
"""

from __future__ import annotations

import json
from pathlib import Path
import tempfile
from typing import Any, Iterable

from .coding_cases import (
    CodingCaseError,
    load_cases,
    provision_validator_dependencies,
)


ROOT = Path(__file__).resolve().parents[2]
CODING_BUILTINS_ROOT = ROOT / "crates" / "tea-luau" / "builtins"
CODING_BUILTIN_NAMES = ("read", "bash", "edit", "find")
CODING_SCHEMA = "tea-quality-coding-run/v1"


class CodingRunError(RuntimeError):
    """A retired live coding-evaluation entry point was requested."""


def _canonical(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")


def coding_bundle_capabilities() -> list[dict[str, Any]]:
    """Describe the closed, provider-free default coding capability surface."""
    for name in CODING_BUILTIN_NAMES:
        manifest_path = CODING_BUILTINS_ROOT / name / "manifest.json"
        try:
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise CodingRunError(f"cannot read default {name} builtin manifest: {error}") from error
        if not isinstance(manifest, dict) or manifest.get("id") != name:
            raise CodingRunError(f"default {name} builtin manifest is invalid")
    return [
        {"name": name, "kind": "tea_coding_bundle", "description": None, "schema": {"type": "object"}}
        for name in CODING_BUILTIN_NAMES
    ]


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


def prepare_cache(*, cache_root: Path, case_ids: Iterable[str] | None = None) -> dict[str, Any]:
    """Populate pinned source and validator caches without invoking a provider."""
    selected = set(case_ids or ())
    cases = [case for case in load_cases() if not selected or case["id"] in selected]
    missing = selected - {case["id"] for case in cases}
    if missing:
        raise CodingRunError(f"unknown coding case(s): {', '.join(sorted(missing))}")
    cached: list[str] = []
    dependency_caches: dict[str, dict[str, Any]] = {}
    for case in cases:
        from .coding_cases import cache_bare_repository

        cache_bare_repository(case["baseline"]["repository"], case["baseline"]["commit"], cache_root, populate=True)
        cache_bare_repository(case["baseline"]["repository"], case["baseline"]["fix_commit"], cache_root, populate=True)
        if case.get("validator_dependencies") is not None:
            with tempfile.TemporaryDirectory(prefix=f"tea-quality-{case['id']}-") as temporary:
                prepared = provision_validator_dependencies(
                    case, cache_root, Path(temporary) / "dependencies", populate_cache=True
                )
            dependency_caches[case["id"]] = {
                key: value for key, value in prepared.items() if key != "node_path"
            }
        cached.append(case["id"])
    return {
        "schema_version": CODING_SCHEMA,
        "operation": "prepare-cache",
        "cases": cached,
        "dependency_caches": dependency_caches,
        "cache_root": str(cache_root),
    }


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
    """Refuse the retired arbitrary-model provider runner before any setup."""
    del model, cache_root, workspace_root, out, validator, env_file, case_ids
    raise CodingRunError(
        "the historical coding runner is retired; use the restricted Rust live-verification "
        "factory and its current BLOCKED evidence instead"
    )
