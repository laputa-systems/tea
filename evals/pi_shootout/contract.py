"""The versioned, adapter-neutral record used by the Pi shootout."""

from __future__ import annotations

import hashlib
import json
import math
from pathlib import Path
from typing import Any


RESULT_SCHEMA = "tea-coding-eval-result/v4"
LEGACY_RESULT_SCHEMA = "tea-coding-eval-result/v3"
BASELINES = ("pi-static", "tea-static", "tea-jit")
STATIC_BASELINES = ("pi-static", "tea-static")
TERMINAL_STATUSES = {"completed", "failed", "cancelled", "aborted"}
HARNESS_DECISIONS = {"not-applicable", "no-change", "rejected", "activated"}
PRE_EDIT_TOOL_GATE_MODES = {"none", "direct-edit-v1", "source-local-v1"}
PRE_EDIT_TOOL_GATE_FIELDS = {
    "mode",
    "blocked_tools",
    "target_restricted_tools",
    "source_local_targets",
    "unlocks_after",
    "same_batch_rule",
    "block_reason_sha256",
}
POST_EDIT_VALIDATION_GATE_MODES = {"none", "unmasked-evidence-v1"}
POST_EDIT_VALIDATION_GATE_FIELDS = {
    "mode",
    "applies_after",
    "qualifies_with",
    "resets_after",
    "same_batch_rule",
    "command_profile",
    "completion_reminder_limit",
    "block_reason_sha256",
    "reminder_sha256",
}
POST_EDIT_VALIDATION_BLOCK_REASON = (
    "Validation evidence requires a direct foreground command whose exit status is visible. "
    "Avoid pipelines and status-suppression wrappers; choose an appropriate workspace-local check."
)
POST_EDIT_VALIDATION_REMINDER = (
    "Before finalizing, run an appropriate workspace-local check after the most recent successful edit. "
    "Run it directly so its exit status is visible; avoid pipelines and status-suppression wrappers. "
    "Choose the check from the task and workspace, address any failure, then finish."
)
VALIDATION_EVIDENCE_FIELDS = {
    "state",
    "edit_generation",
    "qualifying_call_id_sha256",
    "qualifying_arguments_sha256",
    "qualifying_process_exit",
    "candidate_failures",
    "masked_call_blocks",
    "reminders_issued",
    "transitions_sha256",
}
VALIDATION_EVIDENCE_STATES = {"not_required", "satisfied", "missing"}
POST_EDIT_VALIDATION_TRANSITION_TYPE = "post_edit_validation_transition"
POST_EDIT_VALIDATION_TRANSITION_FIELDS = {
    "type",
    "transition",
    "generation",
    "qualifying_call_id_sha256",
    "qualifying_arguments_sha256",
    "process_exit",
}
POST_EDIT_VALIDATION_TRANSITIONS = {
    "edit-pending",
    "candidate-failed",
    "masked-bash-blocked",
    "evidence-satisfied",
    "completion-reminder-issued",
    "evidence-missing",
}


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


def _sha256(value: Any, name: str, *, nullable: bool = False) -> str | None:
    if nullable and value is None:
        return None
    if (
        not isinstance(value, str)
        or len(value) != 64
        or any(character not in "0123456789abcdef" for character in value)
    ):
        raise ContractError(f"{name} must be a lowercase SHA-256 digest")
    return value


def _literal_sha256(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def _pre_edit_tool_gate(value: Any, name: str) -> dict[str, Any]:
    """Validate the explicit model-visible pre-edit workflow policy witness."""
    gate = _object(value, name)
    if set(gate) != PRE_EDIT_TOOL_GATE_FIELDS:
        raise ContractError(f"{name} must contain exactly the pre-edit gate fields")
    mode = _string(gate.get("mode"), f"{name}.mode")
    if mode not in PRE_EDIT_TOOL_GATE_MODES:
        raise ContractError(f"{name}.mode is invalid")
    blocked_tools = _array_of_strings(gate.get("blocked_tools"), f"{name}.blocked_tools")
    target_restricted_tools = _array_of_strings(
        gate.get("target_restricted_tools"),
        f"{name}.target_restricted_tools",
    )
    source_local_targets = _array_of_strings(
        gate.get("source_local_targets"),
        f"{name}.source_local_targets",
    )
    unlocks_after = gate.get("unlocks_after")
    same_batch_rule = gate.get("same_batch_rule")
    reason_digest = gate.get("block_reason_sha256")
    if mode == "none":
        if (
            blocked_tools
            or target_restricted_tools
            or source_local_targets
            or unlocks_after is not None
            or same_batch_rule is not None
            or reason_digest is not None
        ):
            raise ContractError(f"{name} must use only null or empty policy details when disabled")
    elif mode == "direct-edit-v1":
        if blocked_tools != ["bash", "find"]:
            raise ContractError(f"{name}.blocked_tools must be ordered bash, find")
        if target_restricted_tools or source_local_targets:
            raise ContractError(f"{name} direct-edit policy must not declare source-local restrictions")
        if unlocks_after != "prior-successful-edit-result":
            raise ContractError(f"{name}.unlocks_after is invalid")
        if same_batch_rule != "block-until-prior-successful-edit-result":
            raise ContractError(f"{name}.same_batch_rule is invalid")
        if (
            not isinstance(reason_digest, str)
            or len(reason_digest) != 64
            or any(character not in "0123456789abcdef" for character in reason_digest)
        ):
            raise ContractError(f"{name}.block_reason_sha256 must be a lowercase SHA-256 digest")
    else:
        if blocked_tools != ["bash", "find"]:
            raise ContractError(f"{name}.blocked_tools must be ordered bash, find")
        if target_restricted_tools != ["read", "edit"]:
            raise ContractError(f"{name}.target_restricted_tools must be ordered read, edit")
        if not source_local_targets or len(set(source_local_targets)) != len(source_local_targets):
            raise ContractError(f"{name}.source_local_targets must be a non-empty unique array")
        for target in source_local_targets:
            path = Path(target)
            if (
                path.is_absolute()
                or "\\" in target
                or "\x00" in target
                or any(part in ("", ".", "..") for part in target.split("/"))
            ):
                raise ContractError(f"{name}.source_local_targets must contain safe relative paths")
        if unlocks_after != "prior-successful-target-local-edit-result":
            raise ContractError(f"{name}.unlocks_after is invalid")
        if same_batch_rule != "block-until-prior-successful-target-local-edit-result":
            raise ContractError(f"{name}.same_batch_rule is invalid")
        if (
            not isinstance(reason_digest, str)
            or len(reason_digest) != 64
            or any(character not in "0123456789abcdef" for character in reason_digest)
        ):
            raise ContractError(f"{name}.block_reason_sha256 must be a lowercase SHA-256 digest")
    return gate


def _post_edit_validation_gate(value: Any, name: str) -> dict[str, Any]:
    """Validate the shared content-free post-edit workflow policy witness."""
    gate = _object(value, name)
    if set(gate) != POST_EDIT_VALIDATION_GATE_FIELDS:
        raise ContractError(f"{name} must contain exactly the post-edit validation gate fields")
    mode = _string(gate.get("mode"), f"{name}.mode")
    if mode not in POST_EDIT_VALIDATION_GATE_MODES:
        raise ContractError(f"{name}.mode is invalid")
    reminder_limit = _integer(gate.get("completion_reminder_limit"), f"{name}.completion_reminder_limit")
    details = (
        "applies_after",
        "qualifies_with",
        "resets_after",
        "same_batch_rule",
        "command_profile",
        "block_reason_sha256",
        "reminder_sha256",
    )
    if mode == "none":
        if reminder_limit != 0 or any(gate.get(field) is not None for field in details):
            raise ContractError(f"{name} must use only null policy details and a zero reminder limit when disabled")
        return gate
    expected = {
        "applies_after": "prior-successful-declared-target-edit-result",
        "qualifies_with": "prior-successful-unmasked-direct-foreground-bash-result",
        "resets_after": "later-successful-edit-result",
        "same_batch_rule": "evidence-requires-prior-successful-bash-result",
        "command_profile": "unmasked-direct-foreground-bash/v1",
        "completion_reminder_limit": 1,
        "block_reason_sha256": _literal_sha256(POST_EDIT_VALIDATION_BLOCK_REASON),
        "reminder_sha256": _literal_sha256(POST_EDIT_VALIDATION_REMINDER),
    }
    for field, expected_value in expected.items():
        if gate.get(field) != expected_value:
            raise ContractError(f"{name}.{field} is invalid")
    return gate


def _validation_evidence(value: Any, name: str, trace: list[Any]) -> dict[str, Any]:
    """Validate content-free evidence without naming or exposing a host check.

    An ``evidence-satisfied`` transition carries only the content-free
    ``exited-zero`` process witness for its direct foreground bash child. It
    never serializes a raw command, process output, generic tool-success bit,
    or host-validator identity.
    """
    evidence = _object(value, name)
    if set(evidence) != VALIDATION_EVIDENCE_FIELDS:
        raise ContractError(f"{name} must contain exactly the validation evidence fields")
    state = _string(evidence.get("state"), f"{name}.state")
    if state not in VALIDATION_EVIDENCE_STATES:
        raise ContractError(f"{name}.state is invalid")
    generation = evidence.get("edit_generation")
    if generation is not None:
        generation = _integer(generation, f"{name}.edit_generation")
    call_id_digest = _sha256(
        evidence.get("qualifying_call_id_sha256"),
        f"{name}.qualifying_call_id_sha256",
        nullable=True,
    )
    arguments_digest = _sha256(
        evidence.get("qualifying_arguments_sha256"),
        f"{name}.qualifying_arguments_sha256",
        nullable=True,
    )
    process_exit = evidence.get("qualifying_process_exit")
    if process_exit not in {None, "exited-zero"}:
        raise ContractError(f"{name}.qualifying_process_exit is invalid")
    candidate_failures = _integer(evidence.get("candidate_failures"), f"{name}.candidate_failures")
    masked_call_blocks = _integer(evidence.get("masked_call_blocks"), f"{name}.masked_call_blocks")
    reminders_issued = _integer(evidence.get("reminders_issued"), f"{name}.reminders_issued")
    _sha256(evidence.get("transitions_sha256"), f"{name}.transitions_sha256")

    transitions: list[dict[str, Any]] = []
    for ordinal, entry in enumerate(trace, start=1):
        if not isinstance(entry, dict) or entry.get("type") != POST_EDIT_VALIDATION_TRANSITION_TYPE:
            continue
        entry_name = f"trace[{ordinal}]"
        if set(entry) != POST_EDIT_VALIDATION_TRANSITION_FIELDS:
            raise ContractError(f"{entry_name} must contain only content-free post-edit transition fields")
        transition = _string(entry.get("transition"), f"{entry_name}.transition")
        if transition not in POST_EDIT_VALIDATION_TRANSITIONS:
            raise ContractError(f"{entry_name}.transition is invalid")
        _integer(entry.get("generation"), f"{entry_name}.generation")
        _sha256(entry.get("qualifying_call_id_sha256"), f"{entry_name}.qualifying_call_id_sha256", nullable=True)
        _sha256(entry.get("qualifying_arguments_sha256"), f"{entry_name}.qualifying_arguments_sha256", nullable=True)
        entry_process_exit = entry.get("process_exit")
        if transition == "evidence-satisfied":
            if entry_process_exit != "exited-zero":
                raise ContractError(f"{entry_name}.process_exit must attest an exited-zero child")
        elif entry_process_exit is not None:
            raise ContractError(f"{entry_name}.process_exit must be null outside evidence-satisfied")
        transitions.append(entry)

    pending = [entry for entry in transitions if entry["transition"] == "edit-pending"]
    if any(
        later["generation"] <= earlier["generation"]
        for earlier, later in zip(pending, pending[1:])
    ):
        raise ContractError(f"{name} edit-pending generations must increase after later successful native edit results")
    seen_generations: set[int] = set()
    for entry in transitions:
        if entry["transition"] == "edit-pending":
            seen_generations.add(entry["generation"])
            continue
        if entry["generation"] not in seen_generations:
            raise ContractError(f"{name} transition must refer to an earlier successful native edit generation")

    if state == "not_required":
        if (
            generation is not None
            or call_id_digest is not None
            or arguments_digest is not None
            or process_exit is not None
        ):
            raise ContractError(f"{name} not_required state cannot identify an edit or qualifying call")
        if candidate_failures or masked_call_blocks or reminders_issued or transitions:
            raise ContractError(f"{name} not_required state must have no validation activity")
    elif state == "satisfied":
        if (
            generation is None
            or call_id_digest is None
            or arguments_digest is None
            or process_exit != "exited-zero"
        ):
            raise ContractError(f"{name} satisfied state requires one exited-zero qualifying process witness")
        if reminders_issued not in {0, 1}:
            raise ContractError(f"{name} satisfied state allows at most one completion reminder")
        if not pending or pending[-1]["generation"] != generation:
            raise ContractError(f"{name} satisfied state must describe the most recent successful native edit")
        if not any(
            entry["transition"] == "evidence-satisfied"
            and entry["generation"] == generation
            and entry["qualifying_call_id_sha256"] == call_id_digest
            and entry["qualifying_arguments_sha256"] == arguments_digest
            and entry["process_exit"] == "exited-zero"
            for entry in transitions
        ):
            raise ContractError(f"{name} satisfied state requires a matching evidence-satisfied transition")
    else:
        if (
            generation is None
            or call_id_digest is not None
            or arguments_digest is not None
            or process_exit is not None
        ):
            raise ContractError(f"{name} missing state requires an edit generation without a qualifying call")
        if reminders_issued != 1:
            raise ContractError(f"{name} missing state requires exactly one completion reminder")
        if not pending or pending[-1]["generation"] != generation:
            raise ContractError(f"{name} missing state must describe the most recent successful native edit")
        if not any(
            entry["transition"] == "completion-reminder-issued"
            and entry["generation"] == generation
            for entry in transitions
        ) or not any(
            entry["transition"] == "evidence-missing" and entry["generation"] == generation
            for entry in transitions
        ):
            raise ContractError(f"{name} missing state requires completion-reminder and evidence-missing transitions")

    if candidate_failures != sum(entry["transition"] == "candidate-failed" for entry in transitions):
        raise ContractError(f"{name}.candidate_failures must match content-free transition evidence")
    if masked_call_blocks != sum(entry["transition"] == "masked-bash-blocked" for entry in transitions):
        raise ContractError(f"{name}.masked_call_blocks must match content-free transition evidence")
    if reminders_issued != sum(entry["transition"] == "completion-reminder-issued" for entry in transitions):
        raise ContractError(f"{name}.reminders_issued must match content-free transition evidence")
    return evidence


def _validate_result(
    value: Any,
    *,
    schema: str,
    requires_post_edit_validation_evidence: bool,
    attempt_id: str | None = None,
    baseline_id: str | None = None,
) -> dict[str, Any]:
    """Validate one versioned result shape before reports consume it."""
    result = _object(value, "result")
    if result.get("schema_version") != schema:
        raise ContractError(f"schema_version must be {schema!r}")
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
    surface_pre_edit_tool_gate = _pre_edit_tool_gate(
        surface.get("pre_edit_tool_gate"),
        "surface.pre_edit_tool_gate",
    )
    surface_post_edit_validation_gate: dict[str, Any] | None = None
    if requires_post_edit_validation_evidence:
        surface_post_edit_validation_gate = _post_edit_validation_gate(
            surface.get("post_edit_validation_gate"),
            "surface.post_edit_validation_gate",
        )
    elif "post_edit_validation_gate" in surface:
        raise ContractError("legacy v3 surface must not contain post-edit validation gate evidence")
    trace = result.get("trace")
    if not isinstance(trace, list):
        raise ContractError("trace must be an array")
    if requires_post_edit_validation_evidence:
        validation_evidence = _validation_evidence(
            result.get("validation_evidence"),
            "validation_evidence",
            trace,
        )
        if (
            surface_post_edit_validation_gate is not None
            and surface_post_edit_validation_gate["mode"] == "none"
            and validation_evidence["state"] != "not_required"
        ):
            raise ContractError("disabled post-edit validation gate cannot publish validation activity")
    elif "validation_evidence" in result:
        raise ContractError("legacy v3 result must not contain post-edit validation evidence")
    elif any(
        isinstance(entry, dict)
        and entry.get("type") == POST_EDIT_VALIDATION_TRANSITION_TYPE
        for entry in trace
    ):
        raise ContractError("legacy v3 trace must not contain post-edit validation transitions")

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
    controlled_pre_edit_tool_gate = _pre_edit_tool_gate(
        controlled.get("pre_edit_tool_gate"),
        "effective_policy.controlled.pre_edit_tool_gate",
    )
    controlled_post_edit_validation_gate: dict[str, Any] | None = None
    if requires_post_edit_validation_evidence:
        controlled_post_edit_validation_gate = _post_edit_validation_gate(
            controlled.get("post_edit_validation_gate"),
            "effective_policy.controlled.post_edit_validation_gate",
        )
    elif "post_edit_validation_gate" in controlled:
        raise ContractError("legacy v3 controlled policy must not contain post-edit validation gate evidence")
    if surface_pre_edit_tool_gate != controlled_pre_edit_tool_gate:
        raise ContractError("surface and effective policy pre-edit gate evidence differ")
    if (
        requires_post_edit_validation_evidence
        and surface_post_edit_validation_gate != controlled_post_edit_validation_gate
    ):
        raise ContractError("surface and effective policy post-edit validation gate evidence differ")
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
    if (
        surface_post_edit_validation_gate is not None
        and surface_post_edit_validation_gate["mode"] == "unmasked-evidence-v1"
    ):
        if actual_baseline not in STATIC_BASELINES or harness["mode"] != "static":
            raise ContractError("post-edit validation gate is valid only for static baselines")
        if surface_pre_edit_tool_gate["mode"] != "source-local-v1":
            raise ContractError("post-edit validation gate requires the source-local pre-edit gate")
    return result


def validate_result(value: Any, *, attempt_id: str | None = None, baseline_id: str | None = None) -> dict[str, Any]:
    """Validate the current v4 result required from every fresh adapter run."""
    return _validate_result(
        value,
        schema=RESULT_SCHEMA,
        requires_post_edit_validation_evidence=True,
        attempt_id=attempt_id,
        baseline_id=baseline_id,
    )


def validate_enriched_v3_result(
    value: Any,
    *,
    attempt_id: str | None = None,
    baseline_id: str | None = None,
) -> dict[str, Any]:
    """Read only the complete v3 backport emitted before v4 became current."""
    return _validate_result(
        value,
        schema=LEGACY_RESULT_SCHEMA,
        requires_post_edit_validation_evidence=True,
        attempt_id=attempt_id,
        baseline_id=baseline_id,
    )


def validate_legacy_v3_result(
    value: Any,
    *,
    attempt_id: str | None = None,
    baseline_id: str | None = None,
) -> dict[str, Any]:
    """Read only v3 artifacts that wholly predate post-edit evidence."""
    return _validate_result(
        value,
        schema=LEGACY_RESULT_SCHEMA,
        requires_post_edit_validation_evidence=False,
        attempt_id=attempt_id,
        baseline_id=baseline_id,
    )
