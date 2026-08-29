"""The versioned, adapter-neutral record used by the Pi shootout."""

from __future__ import annotations

import hashlib
import json
import math
from pathlib import Path
from typing import Any


RESULT_SCHEMA = "tea-coding-eval-result/v2"
BASELINES = ("pi-static", "tea-static", "tea-jit")
STATIC_BASELINES = ("pi-static", "tea-static")
TERMINAL_STATUSES = {"completed", "failed", "cancelled", "aborted"}
HARNESS_DECISIONS = {"not-applicable", "no-change", "rejected", "activated"}


class ContractError(ValueError):
    """A persisted shootout artifact is malformed or inconsistent."""


def canonical(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")


def digest(value: Any) -> str:
    return hashlib.sha256(canonical(value)).hexdigest()


def file_digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _object(value: Any, name: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ContractError(f"{name} must be an object")
    return value


def _string(value: Any, name: str, *, nullable: bool = False) -> str | None:
    if nullable and value is None:
        return None
    if not isinstance(value, str) or not value:
        raise ContractError(f"{name} must be a non-empty string")
    return value


def _number(value: Any, name: str, *, nullable: bool = False) -> int | float | None:
    if nullable and value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < 0:
        raise ContractError(f"{name} must be a non-negative finite number")
    return value


def _integer(value: Any, name: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ContractError(f"{name} must be a non-negative integer")
    return value


def _array_of_strings(value: Any, name: str) -> list[str]:
    if not isinstance(value, list) or any(not isinstance(item, str) for item in value):
        raise ContractError(f"{name} must be an array of strings")
    return value


def validate_result(value: Any, *, attempt_id: str | None = None, baseline_id: str | None = None) -> dict[str, Any]:
    """Reject ambiguity at the adapter boundary before reports consume a result."""
    result = _object(value, "result")
    if result.get("schema_version") != RESULT_SCHEMA:
        raise ContractError(f"schema_version must be {RESULT_SCHEMA!r}")
    actual_attempt = _string(result.get("attempt_id"), "attempt_id")
    actual_baseline = _string(result.get("baseline_id"), "baseline_id")
    if actual_baseline not in BASELINES:
        raise ContractError("baseline_id is not a shootout condition")
    if attempt_id is not None and actual_attempt != attempt_id:
        raise ContractError("adapter result attempt_id differs from invocation")
    if baseline_id is not None and actual_baseline != baseline_id:
        raise ContractError("adapter result baseline_id differs from invocation")
    terminal = _object(result.get("terminal"), "terminal")
    if terminal.get("status") not in TERMINAL_STATUSES:
        raise ContractError("terminal.status is invalid")
    if terminal.get("code") is not None and not isinstance(terminal["code"], str):
        raise ContractError("terminal.code must be string or null")
    # A terminal model/provider failure may not contain assistant prose. It is
    # still valid benchmark data as long as the adapter publishes a string and
    # a terminal status rather than turning it into an infrastructure failure.
    if not isinstance(result.get("final_text"), str):
        raise ContractError("final_text must be a string")

    runtime = _object(result.get("runtime"), "runtime")
    if runtime.get("implementation") not in {"pi-sdk", "tea"}:
        raise ContractError("runtime.implementation is invalid")
    for name in ("version", "revision"):
        _string(runtime.get(name), f"runtime.{name}")
    if not isinstance(runtime.get("dirty"), bool):
        raise ContractError("runtime.dirty must be boolean")
    _string(runtime.get("dirty_digest"), "runtime.dirty_digest", nullable=True)

    model = _object(result.get("model"), "model")
    if model.get("provider") != "openrouter":
        raise ContractError("model.provider must be openrouter")
    for name in ("requested_model", "thinking_level"):
        _string(model.get(name), f"model.{name}")
    _string(model.get("returned_model"), "model.returned_model", nullable=True)
    _string(model.get("returned_provider"), "model.returned_provider", nullable=True)
    _number(model.get("max_output_tokens"), "model.max_output_tokens", nullable=True)
    sampling = _object(model.get("sampling"), "model.sampling")
    _number(sampling.get("temperature"), "model.sampling.temperature", nullable=True)
    _number(sampling.get("seed"), "model.sampling.seed", nullable=True)
    if sampling.get("source") not in {"provider-default", "adapter-set"}:
        raise ContractError("model.sampling.source is invalid")

    surface = _object(result.get("surface"), "surface")
    _integer(surface.get("system_prompt_bytes"), "surface.system_prompt_bytes")
    for name in ("system_prompt_sha256", "workspace_normalized_system_prompt_sha256", "tool_surface_sha256", "shell_environment_sha256"):
        _string(surface.get(name), f"surface.{name}")
    _array_of_strings(surface.get("active_tools"), "surface.active_tools")
    _array_of_strings(surface.get("research_tools"), "surface.research_tools")
    for name in ("subagents", "shell_curl_available"):
        if not isinstance(surface.get(name), bool):
            raise ContractError(f"surface.{name} must be boolean")

    timings = _object(result.get("timings"), "timings")
    for name in ("agent_ms", "candidate_validation_ms", "rollover_ms"):
        _integer(timings.get(name), f"timings.{name}")
    counts = _object(result.get("counts"), "counts")
    for name in ("turns", "model_turns", "tool_calls", "retries", "compactions"):
        _integer(counts.get(name), f"counts.{name}")
    _number(counts.get("provider_requests"), "counts.provider_requests", nullable=True)
    usage = _object(result.get("usage"), "usage")
    for name in ("input", "prompt_total", "output", "generation", "all_tokens", "cache_read", "cache_write"):
        _number(usage.get(name), f"usage.{name}", nullable=True)
    _number(usage.get("reasoning"), "usage.reasoning", nullable=True)
    if usage["input"] is not None and usage["output"] is not None and usage["generation"] != usage["input"] + usage["output"]:
        raise ContractError("usage.generation must equal input plus output")
    if usage["prompt_total"] is not None and usage["output"] is not None and usage["all_tokens"] != usage["prompt_total"] + usage["output"]:
        raise ContractError("usage.all_tokens must equal prompt total plus output")
    if usage["prompt_total"] is not None and usage["input"] is not None:
        cache_read = usage["cache_read"] or 0
        cache_write = usage["cache_write"] or 0
        if usage["prompt_total"] != usage["input"] + cache_read + cache_write:
            raise ContractError("usage.prompt_total must include input and cache components")
    cost = _object(result.get("cost"), "cost")
    if cost.get("kind") not in {"provider-reported", "catalog-estimate", "unavailable"}:
        raise ContractError("cost.kind is invalid")
    if cost.get("currency") != "USD":
        raise ContractError("cost.currency must be USD")
    _number(cost.get("total"), "cost.total", nullable=True)
    if cost["kind"] == "unavailable" and cost["total"] is not None:
        raise ContractError("unavailable cost cannot have total")

    harness = _object(result.get("harness"), "harness")
    if harness.get("mode") not in {"static", "jit"}:
        raise ContractError("harness.mode is invalid")
    for name in ("base_snapshot_id", "initial_snapshot_id", "final_snapshot_id", "candidate_id", "hypothesis"):
        _string(harness.get(name), f"harness.{name}", nullable=True)
    if harness.get("decision") not in HARNESS_DECISIONS:
        raise ContractError("harness.decision is invalid")
    _integer(harness.get("candidate_count"), "harness.candidate_count")
    _array_of_strings(harness.get("changed_surfaces"), "harness.changed_surfaces")
    _integer(harness.get("candidate_source_bytes"), "harness.candidate_source_bytes")
    if harness["mode"] == "static" and harness["decision"] != "not-applicable":
        raise ContractError("static harness result must be not-applicable")
    if harness["mode"] == "jit" and harness["decision"] == "not-applicable":
        raise ContractError("JIT harness result must record a JIT decision")
    if not isinstance(result.get("trace"), list):
        raise ContractError("trace must be an array")
    return result
