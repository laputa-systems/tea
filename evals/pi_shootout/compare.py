"""Provider-free, apples-to-apples analysis for persisted shootout runs.

The live adapters already publish normalized usage.  This module deliberately
consumes those fields instead of reconstructing token totals from cache
components.  It also reads Tea's durable session evidence when present so an
efficiency investigation can explain extra model turns without hand-parsing a
large JSONL file.
"""

from __future__ import annotations

from collections import Counter
import hashlib
import json
from pathlib import Path
import re
import random
import statistics
from typing import Any

from .contract import ContractError, validate_result


ANALYSIS_SCHEMA = "tea-pi-shootout-analysis/v1"
STATIC_BASELINES = ("pi-static", "tea-static")
EXPECTED_TOOLS = ("read", "bash", "edit", "find")
IDENTITY_FIELDS = (
    "task_id",
    "task_manifest_sha256",
    "baseline_commit",
    "validator_sha256",
    "model",
    "provider",
    "thinking_level",
    "max_output_tokens",
    "timeout_seconds",
)
USAGE_FIELDS = (
    "input",
    "output",
    "generation",
    "prompt_total",
    "all_tokens",
    "cache_read",
    "cache_write",
)
COUNT_FIELDS = ("turns", "model_turns", "provider_requests", "tool_calls", "retries", "compactions")


class ComparisonError(ValueError):
    """The persisted run cannot be analyzed as a shootout artifact."""


def _read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ComparisonError(f"cannot read JSON artifact {path}: {error}") from error
    if not isinstance(value, dict):
        raise ComparisonError(f"JSON artifact {path} must contain an object")
    return value


def _canonical(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")


def _digest(value: Any) -> str:
    return hashlib.sha256(_canonical(value)).hexdigest()


def _attempt_directory(run_dir: Path, attempt_id: str) -> Path | None:
    attempts = run_dir / "attempts"
    if not attempts.is_dir():
        return None
    for record_path in attempts.glob("*/record.json"):
        try:
            record = _read_json(record_path)
        except ComparisonError:
            continue
        if record.get("attempt_id") == attempt_id:
            return record_path.parent
    # A copied summary may not retain record.json.  The runner's names are
    # stable enough to provide a useful fallback for session evidence.
    suffix = attempt_id.split("-", 2)[-1]
    direct = attempts / suffix
    if direct.is_dir():
        return direct
    match = sorted(attempts.glob(f"*-{suffix}"))
    return match[0] if match else None


def _repeat(attempt_id: str) -> int:
    match = re.search(r"-r(\d+)-", attempt_id)
    return int(match.group(1)) if match else 1


def _category(tool_name: str, arguments: Any) -> str:
    """Assign a redacted, stable work category to one tool call."""
    if tool_name in {"read", "find"}:
        return "inspection"
    if tool_name == "edit":
        return "edit"
    if tool_name != "bash" or not isinstance(arguments, dict):
        return "other"
    command = arguments.get("command")
    if not isinstance(command, str):
        return "shell"
    lowered = command.lower()
    if re.search(r"\b(curl|npm\s+(?:view|pack|install)|git\s+fetch)\b|github\.com", lowered):
        return "upstream_or_dependency"
    if re.search(r"\b(npm\s+(?:test|run)|(?:npx|npm|yarn|pnpm)\s+.*(?:mocha|jest)|pytest|cargo\s+test)\b", lowered):
        return "validation"
    if re.search(r"\b(eslint|clippy|rustfmt|lint)\b", lowered):
        return "lint"
    if re.search(r"\bgit\s+(?:status|log|diff|show|stash|branch)\b", lowered):
        return "repository_state"
    return "shell"


def _tool_summary(tool_name: str, arguments: Any) -> dict[str, Any]:
    # Arguments are intentionally represented by a digest.  The comparison
    # artifact is safe to share and still lets investigators detect repeated
    # or identical calls.
    return {"name": tool_name, "category": _category(tool_name, arguments), "arguments_sha256": _digest(arguments)}


def _tea_turns(session_path: Path) -> dict[str, Any] | None:
    if not session_path.is_file():
        return None
    try:
        rows = [json.loads(line) for line in session_path.read_text(encoding="utf-8").splitlines() if line.strip()]
    except (OSError, json.JSONDecodeError) as error:
        raise ComparisonError(f"cannot read Tea session evidence {session_path}: {error}") from error

    provider_requests: dict[str, dict[str, Any]] = {}
    settled_order: list[str] = []
    result_entry_by_step: dict[str, str] = {}
    result_entry_by_request: dict[str, str] = {}
    turns: list[dict[str, Any]] = []
    tool_by_id: dict[str, dict[str, Any]] = {}
    for row in rows:
        mutation = row.get("mutation", {})
        if not isinstance(mutation, dict):
            continue
        payload = mutation.get("payload", {})
        if not isinstance(payload, dict):
            continue
        if mutation.get("kind") == "record" and payload.get("type") == "step_attempted":
            step_id, result_entry_id = payload.get("id"), payload.get("result_entry_id")
            if payload.get("step_kind") == "assistant" and isinstance(step_id, str) and isinstance(result_entry_id, str):
                result_entry_by_step[step_id] = result_entry_id
            continue
        if mutation.get("kind") == "record" and payload.get("type") == "provider_request_started":
            request_id, step_id = payload.get("request_id"), payload.get("step_id")
            if isinstance(request_id, str) and isinstance(step_id, str) and step_id in result_entry_by_step:
                result_entry_by_request[request_id] = result_entry_by_step[step_id]
            continue
        if mutation.get("kind") == "record" and payload.get("type") == "provider_request_settled":
            outcome = payload.get("outcome", {})
            usage = payload.get("usage", {})
            request_id = payload.get("request_id")
            if isinstance(request_id, str):
                provider_requests[request_id] = {
                    "request_id": request_id,
                    "result_entry_id": result_entry_by_request.get(request_id),
                    "stop_reason": outcome.get("stop_reason") if isinstance(outcome, dict) else None,
                    "status": outcome.get("status") if isinstance(outcome, dict) else None,
                    "usage": {name: usage.get(name) if isinstance(usage, dict) else None for name in ("input_tokens", "output_tokens", "reasoning_tokens", "cache_read_tokens", "cache_write_tokens")},
                }
                settled_order.append(request_id)
            continue
        if mutation.get("kind") != "entry":
            continue
        entry = payload.get("entry", {})
        if not isinstance(entry, dict):
            continue
        if entry.get("type") == "assistant_message":
            entry_id = payload.get("id")
            request = next((request for request in provider_requests.values() if request.get("result_entry_id") == entry_id), None) if isinstance(entry_id, str) else None
            tool_calls = []
            for call in entry.get("tool_calls", []):
                if not isinstance(call, dict):
                    continue
                tool_name = call.get("name") if isinstance(call.get("name"), str) else "unknown"
                tool = _tool_summary(tool_name, call.get("arguments", {}))
                tool["tool_call_id"] = call.get("id")
                tool_calls.append(tool)
                if isinstance(call.get("id"), str):
                    tool_by_id[call["id"]] = tool
            content = entry.get("content", "")
            turns.append(
                {
                    "ordinal": len(turns) + 1,
                    "stop_reason": entry.get("stop_reason") or (request or {}).get("stop_reason"),
                    "assistant_text_bytes": len(content.encode("utf-8")) if isinstance(content, str) else 0,
                    "tool_calls": tool_calls,
                    "tool_result_errors": 0,
                    "provider_request": request,
                }
            )
        elif entry.get("type") == "tool_result":
            tool = tool_by_id.get(entry.get("tool_call_id"))
            if tool is not None and entry.get("is_error"):
                # The error count is copied onto the owning turn below.
                tool["is_error"] = True

    for turn in turns:
        errors = sum(1 for call in turn["tool_calls"] if call.get("is_error"))
        turn["tool_result_errors"] = errors
        turn["categories"] = dict(Counter(call["category"] for call in turn["tool_calls"]))
        for call in turn["tool_calls"]:
            call.pop("is_error", None)

    def summary(name: str) -> dict[str, Any] | None:
        values = [provider_requests[request_id]["usage"].get(name) for request_id in settled_order]
        values = [value for value in values if isinstance(value, (int, float))]
        if not values:
            return None
        return {"first": values[0], "last": values[-1], "min": min(values), "max": max(values), "total": sum(values)}

    return {
        "source": "tea-durable-session",
        "complete": bool(turns) and bool(provider_requests) and all(turn.get("provider_request") is not None for turn in turns) and all(request.get("result_entry_id") is not None for request in provider_requests.values()),
        "provider_request_count": len(provider_requests),
        "request_usage": {name: summary(name) for name in ("input_tokens", "output_tokens", "reasoning_tokens", "cache_read_tokens", "cache_write_tokens")},
        "turns": turns,
    }


def _trace_turns(result: dict[str, Any]) -> dict[str, Any]:
    trace = result.get("trace", [])
    turns: list[dict[str, Any]] = []
    current: dict[str, Any] | None = None
    for event in trace:
        if not isinstance(event, dict):
            continue
        event_type = event.get("type")
        if event_type == "turn_start":
            current = {"ordinal": len(turns) + 1, "stop_reason": None, "assistant_text_bytes": None, "tool_calls": [], "tool_result_errors": 0, "categories": {}}
            turns.append(current)
        elif event_type == "tool_execution_start" and current is not None:
            name = event.get("tool_name", "unknown")
            category = event.get("category") if isinstance(event.get("category"), str) else "unknown"
            current["tool_calls"].append({"name": name, "category": category, "arguments_sha256": event.get("arguments_sha256"), "tool_call_id": event.get("tool_call_id")})
        elif event_type == "tool_execution_end" and current is not None and event.get("is_error"):
            current["tool_result_errors"] += 1
    for turn in turns:
        turn["categories"] = dict(Counter(call["category"] for call in turn["tool_calls"]))
    return {"source": "adapter-trace", "complete": False, "provider_request_count": None, "request_usage": None, "turns": turns}


def _attempt_view(run_dir: Path, record: dict[str, Any]) -> dict[str, Any]:
    baseline = record.get("baseline_id")
    result = record.get("adapter_result")
    if baseline not in STATIC_BASELINES or not isinstance(result, dict):
        raise ComparisonError("static comparison requires pi-static and tea-static adapter results")
    try:
        validate_result(result, attempt_id=record.get("attempt_id"), baseline_id=baseline)
    except ContractError as error:
        raise ComparisonError(f"{baseline} result is invalid: {error}") from error
    attempt_id = result["attempt_id"]
    directory = _attempt_directory(run_dir, attempt_id)
    session = None
    if directory is not None:
        session = _tea_turns(directory / "harness" / "session.tea" / "session.jsonl")
    trace = result.get("trace", [])
    event_counts = Counter(event.get("type") for event in trace if isinstance(event, dict))
    turns = session if session is not None else _trace_turns(result)
    tool_name_counts = Counter(call["name"] for turn in turns["turns"] for call in turn["tool_calls"])
    return {
        "baseline_id": baseline,
        "attempt_id": attempt_id,
        "repeat": _repeat(attempt_id),
        "terminal": result["terminal"],
        "validator_passed": bool(record.get("validator", {}).get("passed")),
        "counts": {name: result["counts"].get(name) for name in COUNT_FIELDS},
        "usage": {name: result["usage"].get(name) for name in (*USAGE_FIELDS, "reasoning")},
        "surface": {name: result["surface"].get(name) for name in ("active_tools", "authority", "workspace_normalized_system_prompt_sha256", "system_prompt_sha256", "tool_surface_sha256", "prompt_tool_surface_sha256", "wire_tool_surface_sha256", "execution_surface_sha256", "system_prompt_bytes")},
        "wire": result["wire"],
        "effective_policy": result["effective_policy"],
        "initial_workspace_state": record.get("initial_workspace_state"),
        "trace": {"event_counts": dict(event_counts), "turn_evidence": turns, "tool_name_counts": dict(tool_name_counts)},
    }


def _delta(left: Any, right: Any) -> Any:
    if isinstance(left, (int, float)) and isinstance(right, (int, float)) and not isinstance(left, bool) and not isinstance(right, bool):
        return right - left
    return None


def _pair(pi: dict[str, Any], tea: dict[str, Any]) -> dict[str, Any]:
    return {
        "repeat": pi["repeat"],
        "pi": pi,
        "tea": tea,
        "delta_tea_minus_pi": {
            "usage": {name: _delta(pi["usage"].get(name), tea["usage"].get(name)) for name in USAGE_FIELDS},
            "counts": {name: _delta(pi["counts"].get(name), tea["counts"].get(name)) for name in COUNT_FIELDS},
            "tool_name_counts": {
                name: _delta(pi["trace"]["tool_name_counts"].get(name, 0), tea["trace"]["tool_name_counts"].get(name, 0))
                for name in sorted(set(pi["trace"]["tool_name_counts"]) | set(tea["trace"]["tool_name_counts"]))
            },
        },
    }


def _aggregate(pairs: list[dict[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for field_group, fields in (("usage", USAGE_FIELDS), ("counts", COUNT_FIELDS)):
        result[field_group] = {}
        for field in fields:
            values = [pair["delta_tea_minus_pi"][field_group][field] for pair in pairs]
            values = [value for value in values if isinstance(value, (int, float))]
            result[field_group][field] = {
                "median": statistics.median(values) if values else None,
                "min": min(values) if values else None,
                "max": max(values) if values else None,
                "samples": len(values),
                "paired_observations": values,
                "bootstrap_mean_ci95": _bootstrap_mean_ci(values),
            }
    return result


def _bootstrap_mean_ci(values: list[int | float]) -> dict[str, float] | None:
    """A descriptive tiny-sample interval, not an asymptotic significance test."""
    if not values:
        return None
    randomizer = random.Random(0)
    samples = sorted(sum(randomizer.choice(values) for _ in values) / len(values) for _ in range(2_000))
    return {"low": samples[int(0.025 * (len(samples) - 1))], "high": samples[int(0.975 * (len(samples) - 1))]}


def _first_request(view: dict[str, Any]) -> dict[str, Any] | None:
    requests = view["wire"].get("requests")
    return requests[0] if isinstance(requests, list) and requests and isinstance(requests[0], dict) else None


def _setting_value(request: dict[str, Any], name: str) -> tuple[bool, Any]:
    value = request.get(name)
    if not isinstance(value, dict) or not isinstance(value.get("present"), bool):
        return False, None
    return value["present"], value.get("value")


def _wire_checks(pair: dict[str, Any], checks: dict[str, list[str]]) -> None:
    pi, tea = pair["pi"], pair["tea"]
    pi_request, tea_request = _first_request(pi), _first_request(tea)
    if pi_request is None or tea_request is None:
        checks["observability_unknown"].append(f"repeat {pair['repeat']} is missing first-request wire evidence")
        return
    for baseline, request in (("pi-static", pi_request), ("tea-static", tea_request)):
        names = request.get("tool_names")
        count = request.get("tool_count")
        if names != list(EXPECTED_TOOLS) or count != 4 or len(set(names if isinstance(names, list) else [])) != 4:
            checks["wire_shape_bugs"].append(f"repeat {pair['repeat']} {baseline} wire request does not expose exactly read,bash,edit,find once")
        assistant_reasoning = request.get("assistant_reasoning_content")
        if assistant_reasoning is False:
            checks["wire_shape_bugs"].append(f"repeat {pair['repeat']} {baseline} omits DeepSeek assistant reasoning_content replay")
    for name in ("temperature", "seed", "max_tokens", "max_completion_tokens", "tool_choice", "parallel_tool_calls", "stream", "stream_options"):
        if _setting_value(pi_request, name) != _setting_value(tea_request, name):
            checks["controlled_condition_mismatches"].append(f"repeat {pair['repeat']} wire field {name} differs")
    if pi_request.get("model") != tea_request.get("model"):
        checks["controlled_condition_mismatches"].append(f"repeat {pair['repeat']} requested wire model differs")
    if pi_request.get("reasoning") != tea_request.get("reasoning"):
        checks["controlled_condition_mismatches"].append(f"repeat {pair['repeat']} wire reasoning configuration differs")
    if pi_request.get("provider_routing") != tea_request.get("provider_routing"):
        checks["controlled_condition_mismatches"].append(f"repeat {pair['repeat']} wire provider-routing policy differs")
    if pi["wire"].get("routing_policy") != tea["wire"].get("routing_policy"):
        checks["controlled_condition_mismatches"].append(f"repeat {pair['repeat']} declared provider-routing policy differs")
    for field in ("system_prompt_sha256", "tool_schema_sha256"):
        if pi_request.get(field) != tea_request.get(field):
            checks["native_harness_surface_differences"].append(f"repeat {pair['repeat']} native {field} differs")
    pi_route, tea_route = pi["wire"].get("returned_route", {}), tea["wire"].get("returned_route", {})
    if isinstance(pi_route, dict) and isinstance(tea_route, dict):
        pi_provider, tea_provider = pi_route.get("provider"), tea_route.get("provider")
        if pi_provider is not None and tea_provider is not None and pi_provider != tea_provider:
            checks["route_mismatches"].append(f"repeat {pair['repeat']} OpenRouter returned different underlying providers: {pi_provider!r} / {tea_provider!r}")
        elif pi_provider is None or tea_provider is None:
            checks["observability_unknown"].append(f"repeat {pair['repeat']} underlying OpenRouter provider was not observed for both adapters")
        pi_model, tea_model = pi_route.get("model"), tea_route.get("model")
        if pi_model is not None and tea_model is not None and pi_model != tea_model:
            checks["route_mismatches"].append(f"repeat {pair['repeat']} OpenRouter returned different routed models: {pi_model!r} / {tea_model!r}")
        elif pi_model is None or tea_model is None:
            checks["observability_unknown"].append(f"repeat {pair['repeat']} routed model was not observed for both adapters")


def _policy_checks(pair: dict[str, Any], checks: dict[str, list[str]]) -> None:
    pi, tea = pair["pi"], pair["tea"]
    pi_controlled = pi["effective_policy"].get("controlled", {})
    tea_controlled = tea["effective_policy"].get("controlled", {})
    for name in ("automatic_compaction", "compaction_threshold", "provider_retry", "request_timeout_seconds", "idle_timeout_seconds", "outer_attempt_timeout_seconds", "model_reasoning", "output_token_ceiling", "provider_routing", "sampling"):
        left, right = pi_controlled.get(name), tea_controlled.get(name)
        if left is None or right is None:
            checks["observability_unknown"].append(f"repeat {pair['repeat']} effective policy {name} is not observable for both adapters")
        elif left != right:
            checks["controlled_condition_mismatches"].append(f"repeat {pair['repeat']} effective policy {name} differs")
    for baseline, view in (("pi-static", pi), ("tea-static", tea)):
        unknown = view["effective_policy"].get("observability_unknown", [])
        if isinstance(unknown, list):
            checks["observability_unknown"].extend(f"repeat {pair['repeat']} {baseline} cannot observe {item}" for item in unknown)


def _surface_checks(pair: dict[str, Any], checks: dict[str, list[str]]) -> None:
    pi, tea = pair["pi"], pair["tea"]
    for baseline, view in (("pi-static", pi), ("tea-static", tea)):
        tools = view["surface"].get("active_tools")
        authority = (view["surface"].get("authority") or {}).get("tools") if isinstance(view["surface"].get("authority"), dict) else None
        if tuple(tools or ()) != EXPECTED_TOOLS or tuple(authority or ()) != EXPECTED_TOOLS:
            checks["controlled_condition_mismatches"].append(f"repeat {pair['repeat']} {baseline} does not use the closed read,bash,edit,find authority")
    if pi["surface"].get("system_prompt_sha256") != tea["surface"].get("system_prompt_sha256"):
        checks["native_harness_surface_differences"].append(f"repeat {pair['repeat']} native system prompts differ")
    if pi["surface"].get("tool_surface_sha256") != tea["surface"].get("tool_surface_sha256"):
        checks["native_harness_surface_differences"].append(f"repeat {pair['repeat']} native tool surfaces differ")
    pi_authority = pi["surface"].get("authority")
    tea_authority = tea["surface"].get("authority")
    if not isinstance(pi_authority, dict) or not isinstance(tea_authority, dict):
        checks["controlled_condition_mismatches"].append(f"repeat {pair['repeat']} is missing an explicit shell/secret authority boundary")
    else:
        for name in ("shell", "secret_boundary"):
            if pi_authority.get(name) != tea_authority.get(name):
                checks["controlled_condition_mismatches"].append(f"repeat {pair['repeat']} shell authority field {name} differs")
    for name in ("shell_curl_available", "shell_environment_sha256"):
        if pi["surface"].get(name) != tea["surface"].get(name):
            checks["controlled_condition_mismatches"].append(f"repeat {pair['repeat']} shell environment field {name} differs")


def compare_run(run_dir: Path) -> dict[str, Any]:
    """Analyze one persisted run, preserving an explicit comparability verdict."""
    summary = _read_json(run_dir / "summary.json")
    attempts = summary.get("attempts")
    run = summary.get("run")
    if not isinstance(attempts, list) or not isinstance(run, dict):
        raise ComparisonError("summary.json must contain run and attempts")
    views = [_attempt_view(run_dir, record) for record in attempts if isinstance(record, dict) and record.get("baseline_id") in STATIC_BASELINES]
    by_repeat: dict[int, dict[str, dict[str, Any]]] = {}
    for view in views:
        by_repeat.setdefault(view["repeat"], {})[view["baseline_id"]] = view
    pairs: list[dict[str, Any]] = []
    checks: dict[str, list[str]] = {
        "controlled_condition_mismatches": [],
        "native_harness_surface_differences": [],
        "wire_shape_bugs": [],
        "route_mismatches": [],
        "observability_unknown": [],
    }
    for repeat, values in sorted(by_repeat.items()):
        if set(values) != set(STATIC_BASELINES):
            checks["controlled_condition_mismatches"].append(f"repeat {repeat} does not contain exactly pi-static and tea-static")
            continue
        pairs.append(_pair(values["pi-static"], values["tea-static"]))

    # The run metadata is the shared identity. Check each adapter's model
    # identity against it so a hand-edited summary cannot look comparable
    # merely because both result files are internally valid.
    for field in IDENTITY_FIELDS:
        if field not in run:
            checks["controlled_condition_mismatches"].append(f"run metadata is missing identity field {field}")
    for field in ("provider_routing", "toolchain_manifest_sha256"):
        if field not in run:
            checks["controlled_condition_mismatches"].append(f"run metadata is missing controlled field {field}")
    for record in attempts:
        if not isinstance(record, dict) or record.get("baseline_id") not in STATIC_BASELINES:
            continue
        result = record.get("adapter_result")
        if not isinstance(result, dict):
            continue
        model = result.get("model", {})
        expected = {
            "provider": run.get("provider"),
            "requested_model": run.get("model"),
            "thinking_level": run.get("thinking_level"),
            "max_output_tokens": run.get("max_output_tokens"),
        }
        for name, expected_value in expected.items():
            if model.get(name) != expected_value:
                checks["controlled_condition_mismatches"].append(f"{record['baseline_id']} model identity field {name} differs from run metadata")
        if result.get("wire", {}).get("routing_policy") != run.get("provider_routing"):
            checks["controlled_condition_mismatches"].append(f"{record['baseline_id']} routing policy differs from run metadata")
    if not pairs:
        checks["controlled_condition_mismatches"].append("no complete static pair is available")
    for pair in pairs:
        pi, tea = pair["pi"], pair["tea"]
        if pi["terminal"]["status"] != "completed" or tea["terminal"]["status"] != "completed":
            checks["controlled_condition_mismatches"].append(f"repeat {pair['repeat']} has a non-completed terminal status")
        if not pi["validator_passed"] or not tea["validator_passed"]:
            checks["controlled_condition_mismatches"].append(f"repeat {pair['repeat']} has a validator failure")
        _surface_checks(pair, checks)
        _wire_checks(pair, checks)
        _policy_checks(pair, checks)
        pi_state, tea_state = pi.get("initial_workspace_state"), tea.get("initial_workspace_state")
        if pi_state is not None and tea_state is not None and pi_state != tea_state:
            checks["controlled_condition_mismatches"].append(f"repeat {pair['repeat']} initial workspace fingerprints differ")
        elif pi_state is None or tea_state is None:
            checks["observability_unknown"].append(f"repeat {pair['repeat']} initial workspace fingerprint is missing for one adapter")

    evidence: list[str] = []
    hypotheses: list[str] = []
    unknowns: list[str] = []
    if pairs:
        first = pairs[0]
        generation_delta = first["delta_tea_minus_pi"]["usage"]["generation"]
        model_turn_delta = first["delta_tea_minus_pi"]["counts"]["model_turns"]
        tool_delta = first["delta_tea_minus_pi"]["counts"]["tool_calls"]
        evidence.append(f"Tea generation delta is {generation_delta} tokens on repeat {first['repeat']}.")
        evidence.append(f"Tea model-turn delta is {model_turn_delta}; tool-call delta is {tool_delta}.")
        evidence.append(f"Tool-call name delta is {first['delta_tea_minus_pi']['tool_name_counts']}.")
        for baseline in STATIC_BASELINES:
            evidence_view = first["pi"] if baseline == "pi-static" else first["tea"]
            turn_evidence = evidence_view["trace"]["turn_evidence"]
            if turn_evidence["source"] == "tea-durable-session":
                categories = Counter(call["category"] for turn in turn_evidence["turns"] for call in turn["tool_calls"])
                evidence.append(f"{baseline} durable turn evidence covers {len(turn_evidence['turns'])} turns and categories {dict(categories)}.")
                request_usage = turn_evidence.get("request_usage") or {}
                raw_input = request_usage.get("input_tokens")
                cache_read = request_usage.get("cache_read_tokens")
                if raw_input and cache_read:
                    evidence.append(f"{baseline} request raw input grows {raw_input['first']}→{raw_input['last']} tokens; cache reads grow {cache_read['first']}→{cache_read['last']}.")
    if any(pair["tea"]["trace"]["turn_evidence"]["source"] == "tea-durable-session" for pair in pairs):
        hypotheses.append("Observed extra Tea turns can be attributed to their recorded work categories, but category counts are evidence of behavior, not proof that the runtime caused it.")
    if checks["native_harness_surface_differences"]:
        evidence.append("Native prompt and tool-schema differences are retained as measured harness surfaces; they are not controlled-condition failures in this native-harness shootout.")
    unknowns.append("Provider-default sampling has no shared seed; one paired attempt cannot separate harness effects from model variance.")
    unknowns.extend(checks["observability_unknown"])
    comparable = not checks["controlled_condition_mismatches"] and not checks["wire_shape_bugs"] and not checks["route_mismatches"]
    strict_efficiency_conclusion = comparable and not checks["observability_unknown"]
    return {
        "schema_version": ANALYSIS_SCHEMA,
        "run": {field: run.get(field) for field in IDENTITY_FIELDS if field in run}
        | {
            "run_id": run.get("run_id"),
            "condition_order": run.get("condition_order"),
            "run_class": run.get("run_class"),
            "parallel_repeats": run.get("parallel_repeats"),
            "provider_routing": run.get("provider_routing"),
            "toolchain_manifest_sha256": run.get("toolchain_manifest_sha256"),
            "validator_dependency_lockfile_sha256": run.get("validator_dependency_lockfile_sha256"),
        },
        "comparable": comparable,
        "strict_efficiency_conclusion": strict_efficiency_conclusion,
        "comparability_checks": checks,
        "comparability_reasons": [*checks["controlled_condition_mismatches"], *checks["wire_shape_bugs"], *checks["route_mismatches"]],
        "pairs": pairs,
        "aggregate_delta_tea_minus_pi": _aggregate(pairs),
        "evidence": evidence,
        "hypotheses": hypotheses,
        "unknowns": unknowns,
    }


def _display(value: Any) -> str:
    if value is None:
        return "—"
    if isinstance(value, bool):
        return "yes" if value else "no"
    return str(value)


def render_markdown(analysis: dict[str, Any]) -> str:
    lines = [
        f"# Shootout comparison — {analysis['run'].get('run_id', 'unknown')}",
        "",
        f"- Comparable: **{'yes' if analysis['comparable'] else 'no'}**",
        f"- Strict efficiency conclusion supported: **{'yes' if analysis['strict_efficiency_conclusion'] else 'no'}**",
        f"- Schema: `{analysis['schema_version']}`",
        "",
        "## Per-repeat deltas (Tea − Pi)",
        "",
        "| Repeat | Generation | Uncached input | Output | Model turns | Tool calls | Validator pair |",
        "| ---: | ---: | ---: | ---: | ---: | ---: | --- |",
    ]
    for pair in analysis["pairs"]:
        delta = pair["delta_tea_minus_pi"]
        pi, tea = pair["pi"], pair["tea"]
        lines.append(
            f"| {pair['repeat']} | {_display(delta['usage']['generation'])} | {_display(delta['usage']['input'])} | {_display(delta['usage']['output'])} | {_display(delta['counts']['model_turns'])} | {_display(delta['counts']['tool_calls'])} | {'yes' if pi['validator_passed'] and tea['validator_passed'] else 'no'} |"
        )
    lines.extend(["", "| Repeat | Tool name | Tea − Pi |", "| ---: | --- | ---: |"])
    for pair in analysis["pairs"]:
        for name, value in pair["delta_tea_minus_pi"]["tool_name_counts"].items():
            lines.append(f"| {pair['repeat']} | {name} | {_display(value)} |")
    lines.extend(["", "## Aggregate delta", "", "| Metric | Median | Min | Max | Samples | Paired observations | Bootstrap mean 95% interval |", "| --- | ---: | ---: | ---: | ---: | --- | --- |"])
    for group, fields in (("usage", ("generation", "input", "output", "prompt_total", "all_tokens")), ("counts", ("model_turns", "tool_calls", "provider_requests"))):
        for field in fields:
            value = analysis["aggregate_delta_tea_minus_pi"][group][field]
            interval = value["bootstrap_mean_ci95"]
            interval_text = "—" if interval is None else f"{interval['low']} to {interval['high']}"
            lines.append(f"| {field} | {_display(value['median'])} | {_display(value['min'])} | {_display(value['max'])} | {value['samples']} | {_display(value['paired_observations'])} | {interval_text} |")
    lines.extend(["", "## Turn evidence", ""])
    for pair in analysis["pairs"]:
        for baseline in STATIC_BASELINES:
            view = pair["pi"] if baseline == "pi-static" else pair["tea"]
            turn_evidence = view["trace"]["turn_evidence"]
            lines.append(f"### Repeat {pair['repeat']} — {baseline}")
            lines.append("")
            lines.append(f"Source: `{turn_evidence['source']}`; complete: `{_display(turn_evidence['complete'])}`; provider requests: `{_display(turn_evidence['provider_request_count'])}`.")
            request_usage = turn_evidence.get("request_usage") or {}
            raw_input = request_usage.get("input_tokens")
            cache_read = request_usage.get("cache_read_tokens")
            if raw_input or cache_read:
                lines.append(f"Request raw input: `{_display(raw_input)}`; cache reads: `{_display(cache_read)}`.")
            lines.append("")
            lines.append("| Turn | Tool calls | Categories | Errors | Stop reason |")
            lines.append("| ---: | ---: | --- | ---: | --- |")
            for turn in turn_evidence["turns"]:
                lines.append(f"| {turn['ordinal']} | {len(turn['tool_calls'])} | {_display(turn.get('categories', {}))} | {turn['tool_result_errors']} | {_display(turn.get('stop_reason'))} |")
            lines.append("")
    for title, key in (("Evidence", "evidence"), ("Hypotheses", "hypotheses"), ("Unknowns", "unknowns")):
        lines.extend([f"## {title}", ""])
        lines.extend(f"- {item}" for item in analysis[key])
        lines.append("")
    if analysis["comparability_reasons"]:
        lines.extend(["## Comparability reasons", "", *[f"- {reason}" for reason in analysis["comparability_reasons"]], ""])
    checks = analysis.get("comparability_checks", {})
    for title, key in (("Native harness differences", "native_harness_surface_differences"), ("Wire-shape bugs", "wire_shape_bugs"), ("Route mismatches", "route_mismatches")):
        values = checks.get(key, [])
        if values:
            lines.extend([f"## {title}", "", *[f"- {value}" for value in values], ""])
    return "\n".join(lines)


def write_comparison(analysis: dict[str, Any], output: Path, markdown: Path) -> tuple[Path, Path]:
    output.parent.mkdir(parents=True, exist_ok=True)
    markdown.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(analysis, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    markdown.write_text(render_markdown(analysis), encoding="utf-8")
    return output, markdown
