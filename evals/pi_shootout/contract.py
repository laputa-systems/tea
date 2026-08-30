"""The versioned, adapter-neutral record used by the Pi shootout."""

from __future__ import annotations

import hashlib
import json
import math
from pathlib import Path
from typing import Any


RESULT_SCHEMA = "tea-coding-eval-result/v3"
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
    _string(model.get("returned_model_provenance"), "model.returned_model_provenance", nullable=True)
    _string(model.get("returned_provider_provenance"), "model.returned_provider_provenance", nullable=True)
    _number(model.get("max_output_tokens"), "model.max_output_tokens", nullable=True)
    sampling = _object(model.get("sampling"), "model.sampling")
    _number(sampling.get("temperature"), "model.sampling.temperature", nullable=True)
    _number(sampling.get("seed"), "model.sampling.seed", nullable=True)
    if sampling.get("source") not in {"provider-default", "adapter-set"}:
        raise ContractError("model.sampling.source is invalid")

    surface = _object(result.get("surface"), "surface")
    _integer(surface.get("system_prompt_bytes"), "surface.system_prompt_bytes")
    for name in ("system_prompt_sha256", "workspace_normalized_system_prompt_sha256", "tool_surface_sha256", "prompt_tool_surface_sha256", "execution_surface_sha256", "shell_environment_sha256"):
        _string(surface.get(name), f"surface.{name}")
    _string(surface.get("wire_tool_surface_sha256"), "surface.wire_tool_surface_sha256", nullable=True)
    _array_of_strings(surface.get("active_tools"), "surface.active_tools")
    _array_of_strings(surface.get("research_tools"), "surface.research_tools")
    authority = _object(surface.get("authority"), "surface.authority")
    _array_of_strings(authority.get("tools"), "surface.authority.tools")
    if not isinstance(authority.get("shell"), bool):
        raise ContractError("surface.authority.shell must be boolean")
    _string(authority.get("secret_boundary"), "surface.authority.secret_boundary")
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
        if usage["input"] > usage["prompt_total"]:
            raise ContractError("usage.input cannot exceed prompt total")
        # Providers such as OpenRouter report cache components as part of the
        # raw prompt total. Normally the components partition that total, but
        # an inconsistent response must not make the adapter invent a larger
        # prompt total after saturated subtraction. Preserve the raw total and
        # accept the documented max(..., 0) normalization in that case.
        if cache_read + cache_write <= usage["prompt_total"] and usage["prompt_total"] != usage["input"] + cache_read + cache_write:
            raise ContractError("usage.prompt_total must include input and cache components")
    cost = _object(result.get("cost"), "cost")
    if cost.get("kind") not in {"provider-reported", "catalog-estimate", "unavailable"}:
        raise ContractError("cost.kind is invalid")
    if cost.get("currency") != "USD":
        raise ContractError("cost.currency must be USD")
    _number(cost.get("total"), "cost.total", nullable=True)
    if cost["kind"] == "unavailable" and cost["total"] is not None:
        raise ContractError("unavailable cost cannot have total")

    wire = _object(result.get("wire"), "wire")
    if wire.get("source") != "direct-final-openrouter-boundary":
        raise ContractError("wire.source must identify the direct OpenRouter boundary")
    _number(wire.get("request_count"), "wire.request_count", nullable=True)
    requests = wire.get("requests")
    if not isinstance(requests, list):
        raise ContractError("wire.requests must be an array")
    if wire["request_count"] is not None and wire["request_count"] != len(requests):
        raise ContractError("wire.request_count must equal wire.requests length")
    routing_policy = _object(wire.get("routing_policy"), "wire.routing_policy")
    returned_route = _object(wire.get("returned_route"), "wire.returned_route")
    for name in ("model", "provider", "provenance"):
        _string(returned_route.get(name), f"wire.returned_route.{name}", nullable=True)
    for ordinal, request in enumerate(requests, start=1):
        request = _object(request, f"wire.requests[{ordinal}]")
        if request.get("ordinal") != ordinal:
            raise ContractError("wire request ordinals must be contiguous")
        _string(request.get("canonical_request_sha256"), f"wire.requests[{ordinal}].canonical_request_sha256")
        _string(request.get("model"), f"wire.requests[{ordinal}].model", nullable=True)
        _integer(request.get("message_count"), f"wire.requests[{ordinal}].message_count")
        _array_of_strings(request.get("message_roles"), f"wire.requests[{ordinal}].message_roles")
        if request["message_count"] != len(request["message_roles"]):
            raise ContractError("wire request message count disagrees with roles")
        messages = request.get("messages")
        if not isinstance(messages, list) or len(messages) != request["message_count"]:
            raise ContractError("wire request messages disagree with message count")
        for message_ordinal, message in enumerate(messages, start=1):
            message = _object(message, f"wire.requests[{ordinal}].messages[{message_ordinal}]")
            if message.get("ordinal") != message_ordinal:
                raise ContractError("wire message ordinals must be contiguous")
            for name in ("role", "structural_sha256", "content_sha256"):
                _string(message.get(name), f"wire.requests[{ordinal}].messages[{message_ordinal}].{name}")
        _string(request.get("system_prompt_sha256"), f"wire.requests[{ordinal}].system_prompt_sha256", nullable=True)
        if request.get("assistant_reasoning_content") is not None and not isinstance(request.get("assistant_reasoning_content"), bool):
            raise ContractError("wire request assistant_reasoning_content must be boolean or null")
        _integer(request.get("tool_count"), f"wire.requests[{ordinal}].tool_count")
        names = _array_of_strings(request.get("tool_names"), f"wire.requests[{ordinal}].tool_names")
        if request["tool_count"] != len(names):
            raise ContractError("wire request tool count disagrees with names")
        _string(request.get("tool_schema_sha256"), f"wire.requests[{ordinal}].tool_schema_sha256")
        for name in ("temperature", "seed", "max_tokens", "max_completion_tokens", "tool_choice", "parallel_tool_calls", "stream", "stream_options"):
            setting = _object(request.get(name), f"wire.requests[{ordinal}].{name}")
            if not isinstance(setting.get("present"), bool) or "value" not in setting:
                raise ContractError(f"wire.requests[{ordinal}].{name} must record presence and value")
        routing = request.get("provider_routing")
        if routing is not None and not isinstance(routing, dict):
            raise ContractError("wire request provider_routing must be object or null")
    if not routing_policy:
        raise ContractError("wire.routing_policy must not be empty")

    policy = _object(result.get("effective_policy"), "effective_policy")
    controlled = _object(policy.get("controlled"), "effective_policy.controlled")
    for name in ("automatic_compaction",):
        if not isinstance(controlled.get(name), bool):
            raise ContractError(f"effective_policy.controlled.{name} must be boolean")
    for name in ("compaction_threshold", "request_timeout_seconds", "idle_timeout_seconds", "output_token_ceiling"):
        _number(controlled.get(name), f"effective_policy.controlled.{name}", nullable=True)
    _integer(controlled.get("outer_attempt_timeout_seconds"), "effective_policy.controlled.outer_attempt_timeout_seconds")
    retry = _object(controlled.get("provider_retry"), "effective_policy.controlled.provider_retry")
    if not isinstance(retry.get("enabled"), bool):
        raise ContractError("effective_policy.controlled.provider_retry.enabled must be boolean")
    _integer(retry.get("max_retries"), "effective_policy.controlled.provider_retry.max_retries")
    _string(controlled.get("model_reasoning"), "effective_policy.controlled.model_reasoning")
    _object(controlled.get("provider_routing"), "effective_policy.controlled.provider_routing")
    sampling_policy = _object(controlled.get("sampling"), "effective_policy.controlled.sampling")
    _number(sampling_policy.get("temperature"), "effective_policy.controlled.sampling.temperature", nullable=True)
    _number(sampling_policy.get("seed"), "effective_policy.controlled.sampling.seed", nullable=True)
    _object(policy.get("native"), "effective_policy.native")
    _array_of_strings(policy.get("observability_unknown"), "effective_policy.observability_unknown")

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
