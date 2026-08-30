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

    provider_requests: list[dict[str, Any]] = []
    turns: list[dict[str, Any]] = []
    tool_by_id: dict[str, dict[str, Any]] = {}
    for row in rows:
        mutation = row.get("mutation", {})
        if not isinstance(mutation, dict):
            continue
        payload = mutation.get("payload", {})
        if not isinstance(payload, dict):
            continue
        if mutation.get("kind") == "record" and payload.get("type") == "provider_request_settled":
            outcome = payload.get("outcome", {})
            usage = payload.get("usage", {})
            provider_requests.append(
                {
                    "stop_reason": outcome.get("stop_reason"),
                    "status": outcome.get("status"),
                    "usage": {name: usage.get(name) for name in ("input_tokens", "output_tokens", "reasoning_tokens", "cache_read_tokens", "cache_write_tokens")},
                }
            )
            continue
        if mutation.get("kind") != "entry":
            continue
        entry = payload.get("entry", {})
        if not isinstance(entry, dict):
            continue
        if entry.get("type") == "assistant_message":
            request = provider_requests[len(turns)] if len(turns) < len(provider_requests) else None
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
        values = [request["usage"].get(name) for request in provider_requests]
        values = [value for value in values if isinstance(value, (int, float))]
        if not values:
            return None
        return {"first": values[0], "last": values[-1], "min": min(values), "max": max(values), "total": sum(values)}

    return {
        "source": "tea-durable-session",
        "complete": bool(turns) and len(turns) == len(provider_requests),
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
            current["tool_calls"].append({"name": name, "category": "unknown", "arguments_sha256": None, "tool_call_id": event.get("tool_call_id")})
        elif event_type == "tool_execution_end" and current is not None and event.get("is_error"):
            current["tool_result_errors"] += 1
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
        "surface": {name: result["surface"].get(name) for name in ("active_tools", "workspace_normalized_system_prompt_sha256", "tool_surface_sha256", "system_prompt_bytes")},
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
            }
    return result


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
    reasons: list[str] = []
    for repeat, values in sorted(by_repeat.items()):
        if set(values) != set(STATIC_BASELINES):
            reasons.append(f"repeat {repeat} does not contain exactly pi-static and tea-static")
            continue
        pairs.append(_pair(values["pi-static"], values["tea-static"]))

    # The run metadata is the shared identity. Check each adapter's model
    # identity against it so a hand-edited summary cannot look comparable
    # merely because both result files are internally valid.
    for field in IDENTITY_FIELDS:
        if field not in run:
            reasons.append(f"run metadata is missing identity field {field}")
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
                reasons.append(f"{record['baseline_id']} model identity field {name} differs from run metadata")
    if not pairs:
        reasons.append("no complete static pair is available")
    for pair in pairs:
        pi, tea = pair["pi"], pair["tea"]
        if pi["terminal"]["status"] != "completed" or tea["terminal"]["status"] != "completed":
            reasons.append(f"repeat {pair['repeat']} has a non-completed terminal status")
        if not pi["validator_passed"] or not tea["validator_passed"]:
            reasons.append(f"repeat {pair['repeat']} has a validator failure")
        if pi["surface"]["active_tools"] != tea["surface"]["active_tools"]:
            reasons.append(f"repeat {pair['repeat']} has different active tool names")
        if tuple(pi["surface"]["active_tools"]) != EXPECTED_TOOLS or tuple(tea["surface"]["active_tools"]) != EXPECTED_TOOLS:
            reasons.append(f"repeat {pair['repeat']} does not use the closed {', '.join(EXPECTED_TOOLS)} tool contract")
        if pi["surface"]["workspace_normalized_system_prompt_sha256"] != tea["surface"]["workspace_normalized_system_prompt_sha256"]:
            reasons.append(f"repeat {pair['repeat']} has different normalized system prompts")
        if pi["surface"]["tool_surface_sha256"] != tea["surface"]["tool_surface_sha256"]:
            reasons.append(f"repeat {pair['repeat']} has different tool surfaces")

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
    if any("different normalized system prompts" in reason or "different tool surfaces" in reason for reason in reasons):
        hypotheses.append("Different model-facing prompt/tool surfaces are a plausible source of trajectory divergence and must be controlled before claiming a runtime effect.")
    unknowns.append("Provider-default sampling has no shared seed; one paired attempt cannot separate harness effects from model variance.")
    unknowns.append("Pi exposes only redacted adapter trace evidence here, so per-request usage and tool arguments are unavailable for exact turn alignment.")

    comparable = not reasons
    return {
        "schema_version": ANALYSIS_SCHEMA,
        "run": {field: run.get(field) for field in IDENTITY_FIELDS if field in run} | {"run_id": run.get("run_id"), "condition_order": run.get("condition_order")},
        "comparable": comparable,
        "comparability_reasons": reasons,
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
    lines.extend(["", "## Aggregate delta", "", "| Metric | Median | Min | Max | Samples |", "| --- | ---: | ---: | ---: | ---: |"])
    for group, fields in (("usage", ("generation", "input", "output", "prompt_total", "all_tokens")), ("counts", ("model_turns", "tool_calls", "provider_requests"))):
        for field in fields:
            value = analysis["aggregate_delta_tea_minus_pi"][group][field]
            lines.append(f"| {field} | {_display(value['median'])} | {_display(value['min'])} | {_display(value['max'])} | {value['samples']} |")
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
    return "\n".join(lines)


def write_comparison(analysis: dict[str, Any], output: Path, markdown: Path) -> tuple[Path, Path]:
    output.parent.mkdir(parents=True, exist_ok=True)
    markdown.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(analysis, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    markdown.write_text(render_markdown(analysis), encoding="utf-8")
    return output, markdown
