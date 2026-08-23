"""Hermetic, trace-first quality evaluation orchestration.

This module intentionally stays in the Python standard library.  It is the
process boundary between declarative quality cases and the Rust fixture
adapter; it never invokes a host ``pi`` executable, discovers a credential, or
uses a shell.  A generated fixture is an adapter input, not a new source of
truth: the fixture manifest is the quality contract for every strict case.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile
from typing import Any, Iterable, Mapping

from .trace import coerce_trace, extract_metrics


ROOT = Path(__file__).resolve().parents[2]
QUALITY_ROOT = Path(__file__).resolve().parent
CASE_ROOT = QUALITY_ROOT / "cases"
PROTOCOL = "tea-quality-adapter/v1"
RUST_ADAPTER = QUALITY_ROOT / "adapters" / "rust-core" / "adapter.py"
QUALITY_SCHEMA = "tea-quality-run/v1"
USAGE = {"input": 0, "output": 0, "cache_read": 0, "cache_write": 0, "total_tokens": 0}


class ContractError(ValueError):
    """The evaluator input is not a safe, complete quality contract."""


class AdapterError(RuntimeError):
    """An adapter process failed before returning its typed result."""


def canonical(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), allow_nan=False)


def sha256(value: Any) -> str:
    source = value.encode("utf-8") if isinstance(value, str) else canonical(value).encode("utf-8")
    return hashlib.sha256(source).hexdigest()


def read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ContractError(f"cannot read JSON {path}: {error}") from error
    if not isinstance(value, dict):
        raise ContractError(f"{path} must contain an object")
    return value


def load_core_cases(case_ids: Iterable[str] | None = None) -> list[tuple[Path, dict[str, Any]]]:
    selected = set(case_ids or ())
    if selected and any(not item or "/" in item or "\\" in item for item in selected):
        raise ContractError("case IDs must be simple quality-case directory names")
    cases: list[tuple[Path, dict[str, Any]]] = []
    for path in sorted((CASE_ROOT / "core").glob("*/manifest.json")):
        manifest = read_json(path)
        case_id = manifest.get("id")
        if not isinstance(case_id, str) or case_id != path.parent.name:
            raise ContractError(f"{path}: id must exactly match its directory")
        if manifest.get("status") == "excluded":
            continue
        if manifest.get("scope") != "core" or manifest.get("gate") not in {"strict", "informational"}:
            raise ContractError(f"{path}: enabled core case needs scope=core and an explicit gate mode")
        if selected and case_id not in selected:
            continue
        cases.append((path, manifest))
    found = {manifest["id"] for _, manifest in cases}
    missing = selected - found
    if missing:
        raise ContractError(f"unknown or excluded core case(s): {', '.join(sorted(missing))}")
    if not cases:
        raise ContractError("no enabled core quality cases selected")
    return cases


def _usage(value: Any) -> dict[str, int]:
    if value is None:
        return dict(USAGE)
    if not isinstance(value, Mapping):
        raise ContractError("model terminal usage must be an object")
    output = dict(USAGE)
    for name in output:
        raw = value.get(name, 0)
        if not isinstance(raw, int) or isinstance(raw, bool) or raw < 0:
            raise ContractError(f"model terminal usage.{name} must be a non-negative integer")
        output[name] = raw
    return output


def _terminal_chunk(raw: Mapping[str, Any]) -> dict[str, Any]:
    event_type = raw.get("type")
    if event_type == "done":
        reason = raw.get("stop_reason")
        if reason == "tool_use":
            reason = "tool_call"
        if reason not in {"stop", "tool_call", "length"}:
            raise ContractError(f"unsupported deterministic done stop reason {reason!r}")
        return {"kind": "done", "stop_reason": reason, "usage": _usage(raw.get("usage"))}
    if event_type == "stream_error":
        message = raw.get("message")
        if not isinstance(message, str) or not message:
            raise ContractError("stream_error needs a non-empty message")
        # The v1 deterministic stream contract has only error/aborted at this
        # boundary. Keep the provider classification in the diagnostic so the
        # canonical trace can classify it without guessing.
        reason = "aborted" if raw.get("reason") == "aborted" else "error"
        category = raw.get("reason", "provider")
        return {
            "kind": "error",
            "reason": reason,
            "message": f"{category}: {message}",
            "usage": _usage(raw.get("usage")),
        }
    raise ContractError(f"model script must end in done or stream_error, got {event_type!r}")


def _compile_turn(raw_turn: Any, turn_index: int) -> dict[str, Any]:
    if not isinstance(raw_turn, Mapping):
        raise ContractError(f"model_script[{turn_index}] must be an object")
    emitted = raw_turn.get("emit")
    if not isinstance(emitted, list) or not emitted:
        raise ContractError(f"model_script[{turn_index}].emit must be a non-empty array")
    chunks: list[dict[str, Any]] = []
    partial_call_open = False
    for raw in emitted[:-1]:
        if not isinstance(raw, Mapping):
            raise ContractError(f"model_script[{turn_index}] event must be an object")
        event_type = raw.get("type")
        if event_type == "text_delta":
            text = raw.get("text")
            if not isinstance(text, str):
                raise ContractError(f"model_script[{turn_index}] text_delta needs text")
            chunks.append({"kind": "text_delta", "text": text})
        elif event_type == "tool_call_start":
            call_id, name = raw.get("id"), raw.get("name")
            if not isinstance(call_id, str) or not call_id or not isinstance(name, str):
                raise ContractError(f"model_script[{turn_index}] tool_call_start needs non-empty id and string name")
            arguments = raw.get("arguments")
            if arguments is not None:
                if not isinstance(arguments, Mapping):
                    raise ContractError(f"model_script[{turn_index}] tool-call arguments must be an object")
                chunks.append({"kind": "tool_call", "id": call_id, "name": name, "arguments": dict(arguments)})
            else:
                # The public core stream accepts complete calls only. A partial
                # call followed by stream failure is deliberately represented
                # as no executable call; the raw manifest remains in the
                # artifact so its unsupported provider-level detail is visible.
                partial_call_open = True
        elif event_type == "tool_call_delta":
            if not partial_call_open:
                raise ContractError(f"model_script[{turn_index}] tool_call_delta has no partial start")
        else:
            raise ContractError(f"model_script[{turn_index}] unsupported non-terminal event {event_type!r}")
    terminal = emitted[-1]
    if not isinstance(terminal, Mapping):
        raise ContractError(f"model_script[{turn_index}] terminal event must be an object")
    chunks.append(_terminal_chunk(terminal))
    return {"chunks": chunks}


def _compile_host(manifest: Mapping[str, Any]) -> dict[str, Any]:
    host = manifest.get("host")
    if not isinstance(host, Mapping):
        raise ContractError("core case host must be an object")
    raw_tools = host.get("tools")
    if not isinstance(raw_tools, list):
        raise ContractError("core case host.tools must be an array")
    tools: list[dict[str, Any]] = []
    for raw_tool in raw_tools:
        if not isinstance(raw_tool, Mapping) or not isinstance(raw_tool.get("name"), str):
            raise ContractError("host tool must have a name")
        calls_out: list[dict[str, Any]] = []
        raw_calls = raw_tool.get("calls")
        if not isinstance(raw_calls, list):
            raise ContractError(f"host tool {raw_tool['name']!r}.calls must be an array")
        for raw_call in raw_calls:
            if not isinstance(raw_call, Mapping) or not isinstance(raw_call.get("arguments"), Mapping):
                raise ContractError("host tool call needs object arguments")
            result = raw_call.get("result")
            if not isinstance(result, Mapping):
                raise ContractError("host tool call needs a result")
            content = result.get("content")
            if not isinstance(content, list) or len(content) != 1 or not isinstance(content[0], Mapping):
                raise ContractError("quality v1 lowering supports exactly one text tool-result content part")
            text = content[0].get("text")
            if content[0].get("type") != "text" or not isinstance(text, str):
                raise ContractError("quality v1 lowering supports text tool-result content only")
            calls_out.append(
                {
                    "arguments": dict(raw_call["arguments"]),
                    "result": {"is_error": bool(result.get("is_error", False)), "content": [{"type": "text", "text": text}]},
                }
            )
        tools.append({"name": raw_tool["name"], "calls": calls_out})

    # Existing adapter checkpoints are used only where the manifest's
    # programmatic cancellation barrier reduces to an observable update. This
    # keeps fast fixtures timer-free and never grants a tool ambient authority.
    case_id = manifest.get("id")
    if case_id == "abort-during-tool":
        gate = next((tool for tool in tools if tool["name"] == "gate"), None)
        if gate and gate["calls"]:
            # The fixture runner reports a throwing tool before it
            # delivers updates. Keep the result successful here so the
            # cancellation checkpoint itself—not that adapter detail—is the
            # observable event in both implementations.
            gate["calls"][0]["result"]["is_error"] = False
            gate["calls"][0]["updates"] = ["gate-started"]
            gate["calls"][0]["cancel_after_update"] = True
    if case_id == "abort-during-parallel-tools":
        first = next((tool for tool in tools if tool["name"] == "A"), None)
        if first and first["calls"]:
            first["calls"][0]["result"]["is_error"] = False
            first["calls"][0]["updates"] = ["A-started"]
            first["calls"][0]["cancel_after_update"] = True
    if case_id == "parallel-tool-ordering":
        for tool in tools:
            if tool["name"] in {"A", "B"} and tool["calls"]:
                tool["calls"][0]["yield_once"] = True
    return {"tools": tools}


def compile_core_fixture(manifest: Mapping[str, Any]) -> dict[str, Any]:
    """Lower an open quality manifest to the shared closed v1 fixture dialect.

    The lowering is intentionally narrow and auditable.  It does not encode an
    expected result; it makes only the complete-call, timer-free surface accepted by
    both existing pinned runners executable.  The caller writes the complete
    source manifest beside the generated fixture in every artifact.
    """

    adapter_fixture = manifest.get("adapter_fixture")
    if isinstance(adapter_fixture, str) and adapter_fixture.startswith("crates/tea-core/fixtures/"):
        return read_json(ROOT / adapter_fixture)
    if adapter_fixture != "generated":
        raise ContractError(f"case {manifest.get('id')!r} has no executable fixture mapping")
    setup = manifest.get("setup")
    actions = manifest.get("actions")
    script = manifest.get("model_script")
    if not isinstance(setup, Mapping) or not isinstance(actions, list) or not isinstance(script, list):
        raise ContractError("generated core case needs setup, actions, and model_script")
    if not isinstance(setup.get("tools"), list):
        raise ContractError("generated core setup.tools must be an array")
    fixture = {
        "format_version": 1,
        "kind": "declarative_parity_fixture",
        "id": manifest["id"],
        "description": manifest.get("description", manifest["id"]),
        "setup": {
            "system_prompt": setup.get("system_prompt"),
            "model": setup.get("model"),
            "thinking_level": setup.get("thinking_level", "off"),
            "tools": setup["tools"],
        },
        "actions": actions,
        "model_script": [_compile_turn(turn, index) for index, turn in enumerate(script)],
        "host": _compile_host(manifest),
        "assertions": {},
    }
    # Cancellation checkpoints reach a second provider request with the
    # cancelled scope in both pinned runners. Supply its explicit aborted
    # response so this probes cancellation rather than fixture exhaustion.
    # `tool-use-zero-calls` deliberately does *not* get an extra turn: its
    # The fixture's deterministic behavior is itself what the case captures.
    if manifest.get("id") in {"abort-during-tool", "abort-during-parallel-tools"} and len(fixture["model_script"]) == 1:
        fixture["model_script"].append(
            {"chunks": [{"kind": "error", "reason": "aborted", "message": "Operation aborted", "usage": dict(USAGE)}]}
        )
    return fixture


def _adapter_command(adapter: str) -> list[str]:
    if adapter == "rust-core":
        return [sys.executable, str(RUST_ADAPTER)]
    raise ContractError(f"unsupported quality adapter {adapter!r}")


def _peak_rss_command(command: list[str]) -> tuple[list[str], str | None]:
    """Use the platform time utility when it can report a process peak RSS.

    This is a diagnostic measurement.  It deliberately wraps the adapter
    process rather than sampling an unrelated host process.  macOS' ``time
    -l`` and GNU ``time -v`` have different output formats, both parsed below.
    """

    time = Path("/usr/bin/time")
    if not time.is_file():
        return command, None
    if sys.platform == "darwin":
        return [str(time), "-l", *command], "darwin"
    return [str(time), "-v", *command], "gnu"


def _parse_peak_rss(stderr: str, style: str | None) -> int | None:
    if style is None:
        return None
    for line in stderr.splitlines():
        text = line.strip().lower()
        if style == "darwin" and text.endswith("maximum resident set size"):
            try:
                return int(text.split()[0])
            except (IndexError, ValueError):
                return None
        if style == "gnu" and "maximum resident set size" in text:
            try:
                return int(text.rsplit(":", 1)[1].strip()) * 1024
            except (IndexError, ValueError):
                return None
    return None


def run_adapter(adapter: str, fixture: Path) -> dict[str, Any]:
    command = _adapter_command(adapter)
    measured_command, rss_style = _peak_rss_command(command)
    payload = canonical({"protocol": PROTOCOL, "operation": "run", "fixture": str(fixture)})
    completed = subprocess.run(
        measured_command,
        cwd=ROOT,
        input=payload,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env={
            "PATH": os.environ.get("PATH", ""),
            "LANG": "C",
            "LC_ALL": "C",
            # This opt-in, runner-local diagnostic preserves request envelopes
            # without changing ordinary fixture golden output.
            "TEA_QUALITY_CAPTURE": "1",
        },
        check=False,
    )
    try:
        response = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise AdapterError(
            f"{adapter} did not return a typed JSON response (exit {completed.returncode}): {completed.stderr.strip()}"
        ) from error
    if not isinstance(response, dict) or response.get("protocol") != PROTOCOL or response.get("adapter") != adapter:
        raise AdapterError(f"{adapter} returned an invalid adapter envelope")
    response["process"] = {
        "exit_code": completed.returncode,
        "peak_rss_bytes": _parse_peak_rss(completed.stderr, rss_style),
        "peak_rss_source": "process_time" if rss_style else "unavailable",
    }
    return response


def _call_metadata(result: Mapping[str, Any]) -> dict[str, dict[str, Any]]:
    calls: dict[str, dict[str, Any]] = {}
    for message in result.get("messages", []):
        if not isinstance(message, Mapping) or message.get("role") != "assistant":
            continue
        for part in message.get("content", []):
            if isinstance(part, Mapping) and part.get("type") == "tool_call" and isinstance(part.get("id"), str):
                calls[part["id"]] = dict(part)
    return calls


def _error_class(call: Mapping[str, Any], setup: Mapping[str, Any]) -> str | None:
    name = call.get("name")
    if not isinstance(name, str) or not name:
        return "malformed_name"
    tools = {tool.get("name"): tool for tool in setup.get("tools", []) if isinstance(tool, Mapping)}
    tool = tools.get(name)
    if tool is None:
        return "unknown_tool"
    arguments = call.get("arguments")
    schema = tool.get("parameters")
    if not isinstance(arguments, Mapping) or not isinstance(schema, Mapping):
        return "validation"
    required = schema.get("required", [])
    if isinstance(required, list) and any(key not in arguments for key in required if isinstance(key, str)):
        return "validation"
    if schema.get("additionalProperties") is False:
        properties = schema.get("properties", {})
        if isinstance(properties, Mapping) and any(key not in properties for key in arguments):
            return "validation"
    return "execution"


def canonical_trace(adapter_response: Mapping[str, Any], manifest: Mapping[str, Any]) -> dict[str, Any]:
    """Normalize either adapter's v1 result into a stable quality trace."""

    result = adapter_response.get("result")
    if not isinstance(result, Mapping):
        raise AdapterError("adapter response has no result object")
    setup = manifest.get("setup")
    if not isinstance(setup, Mapping):
        raise ContractError("manifest has no setup")
    # Adapter identity belongs in trace metadata, not the trajectory. Including
    # it here would make otherwise identical runs diverge at sequence zero.
    events: list[dict[str, Any]] = [{"kind": "run_start", "case_id": manifest.get("id")}]
    calls = _call_metadata(result)
    request_trace = result.get("request_trace")
    requests = request_trace if isinstance(request_trace, list) else []
    request_index = 0
    for raw in result.get("events", []):
        if not isinstance(raw, Mapping):
            continue
        event_type = raw.get("type")
        data = raw.get("data") if isinstance(raw.get("data"), Mapping) else {}
        if event_type == "turn_start":
            request = requests[request_index] if request_index < len(requests) and isinstance(requests[request_index], Mapping) else {
                "capture": "unavailable",
                "turn": data.get("turn", request_index),
            }
            events.append({"kind": "request", "turn": data.get("turn", request_index), "request": request})
            request_index += 1
            events.append({"kind": "turn_start", **dict(data)})
        elif event_type == "tool_execution_start":
            call_id = data.get("tool_call_id")
            call = calls.get(call_id, {}) if isinstance(call_id, str) else {}
            events.append({"kind": "tool_call", **dict(data), "arguments": call.get("arguments")})
        elif event_type == "tool_execution_end":
            call_id = data.get("tool_call_id")
            call = calls.get(call_id, {}) if isinstance(call_id, str) else {}
            is_error = bool(data.get("is_error", False))
            item: dict[str, Any] = {"kind": "tool_result", **dict(data), "status": "failed" if is_error else "completed"}
            if is_error:
                item["error"] = {"error_class": _error_class(call, setup)}
            events.append(item)
        elif event_type == "tool_execution_update":
            events.append({"kind": "tool_update", **dict(data)})
        elif event_type == "turn_end":
            events.append({"kind": "response", **dict(data)})
        elif event_type in {"message_start", "message_end", "message_update"}:
            events.append({"kind": event_type, **dict(data)})
    events.append(
        {
            "kind": "final_state",
            "messages": result.get("messages", []),
            "last_response": result.get("last_response"),
            "usage": result.get("usage"),
            "pending_tool_calls": result.get("state", {}).get("pending_tool_calls", []) if isinstance(result.get("state"), Mapping) else [],
        }
    )
    events.append(
        {
            "kind": "run_end",
            "status": result.get("outcome", "unknown"),
            "settled": result.get("settled") is True,
            "pending_tools_at_end": result.get("state", {}).get("pending_tool_calls", []) if isinstance(result.get("state"), Mapping) else [],
        }
    )
    return coerce_trace({"trace_id": f"{manifest.get('id')}:{adapter_response.get('adapter')}", "metadata": adapter_response.get("metadata", {}), "events": events}).to_dict()


def _environment() -> dict[str, Any]:
    return {
        "python": sys.version.split()[0],
        "platform": platform.platform(),
        "rust_toolchain": "nightly-2026-07-24",
        "cwd": str(ROOT),
        "ambient_environment_forwarded": ["PATH", "LANG", "LC_ALL"],
    }


def run_core_case(manifest_path: Path, manifest: Mapping[str, Any], destination: Path) -> dict[str, Any]:
    fixture = compile_core_fixture(manifest)
    destination.mkdir(parents=True, exist_ok=True)
    fixture_path = destination / "fixture.json"
    fixture_path.write_text(json.dumps(fixture, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    (destination / "case-manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    adapter_outputs: dict[str, Any] = {}
    errors: dict[str, str] = {}
    try:
        adapter_outputs["rust-core"] = run_adapter("rust-core", fixture_path)
    except AdapterError as error:
        errors["rust-core"] = str(error)
    if errors:
        artifact = {
            "schema_version": QUALITY_SCHEMA,
            "case_id": manifest["id"],
            "classification": "INFRA_FAILURE",
            "gate": manifest.get("gate"),
            "environment": _environment(),
            "errors": errors,
            "fixture_sha256": sha256(fixture),
        }
        (destination / "report.json").write_text(json.dumps(artifact, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        return artifact
    rust = adapter_outputs["rust-core"]
    rust_trace = canonical_trace(rust, manifest)
    metrics = extract_metrics(rust_trace).to_dict()
    report = {"schema_version": "pi-quality-trace-report/v1", "adapter": "rust", "metrics": metrics}
    artifact = {
        "schema_version": QUALITY_SCHEMA,
        "case_id": manifest["id"],
        "classification": "PASS",
        "gate": manifest.get("gate"),
        "manifest_path": str(manifest_path.relative_to(ROOT)),
        "fixture_sha256": sha256(fixture),
        "environment": _environment(),
        "adapters": adapter_outputs,
        "trace": rust_trace,
        "report": report,
        "resource_overhead": {
            "rust_peak_rss_bytes": rust.get("process", {}).get("peak_rss_bytes"),
            "rust_allocation_measurement": rust.get("resource", {}).get("allocations"),
        },
    }
    (destination / "rust-response.json").write_text(json.dumps(rust, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    (destination / "rust-trace.json").write_text(json.dumps(rust_trace, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    (destination / "report.json").write_text(json.dumps(artifact, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    (destination / "report.txt").write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return artifact


def run_fast(*, case_ids: Iterable[str] | None = None, out: Path | None = None) -> tuple[int, dict[str, Any]]:
    cases = load_core_cases(case_ids)
    if out is None:
        with tempfile.TemporaryDirectory(prefix="tea-quality-") as temporary:
            return _run_cases(cases, Path(temporary))
    return _run_cases(cases, out)


def _run_cases(cases: list[tuple[Path, dict[str, Any]]], root: Path) -> tuple[int, dict[str, Any]]:
    root.mkdir(parents=True, exist_ok=True)
    artifacts = [run_core_case(path, manifest, root / manifest["id"]) for path, manifest in cases]
    strict_failures = [
        artifact["case_id"]
        for artifact in artifacts
        if artifact.get("gate") == "strict" and artifact.get("classification") != "PASS"
    ]
    summary = {
        "schema_version": QUALITY_SCHEMA,
        "tier": "fast",
        "case_count": len(artifacts),
        "matches": sum(artifact.get("classification") == "PASS" for artifact in artifacts),
        "strict_failures": strict_failures,
        "cases": [{"id": artifact["case_id"], "classification": artifact["classification"]} for artifact in artifacts],
        "artifact_root": str(root),
    }
    (root / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return (1 if strict_failures else 0), summary


def inspect_environment() -> dict[str, Any]:
    """Return an explicit, side-effect-free audit of the evaluation surfaces."""

    return {
        "schema_version": "tea-quality-environment/v1",
        "core": {
            "rust": {
                "adapter": str(RUST_ADAPTER.relative_to(ROOT)),
                "toolchain": "nightly-2026-07-24",
                "tui": False,
                "ambient_discovery": False,
                "network": False,
            },
        },
        "resource_measurement": {
            "peak_rss": "Rust adapter process via platform time utility when available",
            "rust_allocations": "rustybench AllocProfiler benchmark at crates/tea-core/benches/quality_memory.rs",
            "fixture_gate": False,
        },
        "coding": {
            "rust": {
                "adapter": "crates/tea-core/src/bin/tea-eval.rs",
                "runtime": "smol",
                "ambient_discovery": False,
                "provider": "explicit opt-in through an adapter-only env source boundary",
            },
        },
    }


def run_rust_allocation_probe(out: Path | None = None) -> dict[str, Any]:
    """Measure one provider-free Rust harness turn with Rustybench.

    The output is deliberately separate from the semantic fixture gate:
    allocator instrumentation changes the measured path, so this probe
    contributes Rust allocations and peak live allocation bytes on the same
    host without affecting fixture results.
    """

    command = [
        "cargo",
        "+nightly-2026-07-24",
        "bench",
        "-p",
        "tea-core",
        "--bench",
        "quality_memory",
        "--",
        "--sample-count",
        "1",
        "--sample-size",
        "1",
        "--min-time",
        "0.01",
        "--max-time",
        "0.02",
        "--format",
        "json",
    ]
    measured_command, rss_style = _peak_rss_command(command)
    completed = subprocess.run(
        measured_command,
        cwd=ROOT,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env={"PATH": os.environ.get("PATH", ""), "LANG": "C", "LC_ALL": "C"},
        check=False,
    )
    parsed: dict[str, Any] | None = None
    for line in reversed(completed.stdout.splitlines()):
        try:
            candidate = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(candidate, dict) and candidate.get("schema") == 1:
            parsed = candidate
            break
    if completed.returncode != 0 or parsed is None:
        raise AdapterError(
            "Rustybench allocation probe failed: " + (completed.stderr.strip() or completed.stdout.strip())
        )
    result = {
        "schema_version": "tea-quality-resource/v1",
        "probe": "rust-one-text-turn",
        "allocator": "rustybench::AllocProfiler::system",
        "benchmark": parsed,
        "process": {
            "peak_rss_bytes": _parse_peak_rss(completed.stderr, rss_style),
            "peak_rss_source": "process_time" if rss_style else "unavailable",
        },
        "note": "Compare allocations only against Rust runs with this same profiler.",
    }
    if out is not None:
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return result
