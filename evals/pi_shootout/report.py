"""Small, auditable Markdown reports for one shootout run."""

from __future__ import annotations

from pathlib import Path
from typing import Any


def _value(value: Any) -> str:
    if value is None:
        return "—"
    if value is True:
        return "yes"
    if value is False:
        return "no"
    return str(value)


def _attempt(summary: dict[str, Any], baseline: str) -> dict[str, Any]:
    records = [record for record in summary["attempts"] if record["baseline_id"] == baseline]
    if len(records) != 1:
        raise ValueError(f"report needs exactly one {baseline} attempt")
    return records[0]


def _result(record: dict[str, Any]) -> dict[str, Any]:
    result = record.get("adapter_result")
    if not isinstance(result, dict):
        raise ValueError("report cannot render missing adapter result")
    return result


def _rows(left: dict[str, Any], right: dict[str, Any], reference: dict[str, Any] | None = None) -> list[str]:
    fields = [
        ("Validator pass", lambda record: record["validator"]["passed"]),
        ("Terminal status", lambda record: _result(record)["terminal"]["status"]),
        ("Terminal code", lambda record: _result(record)["terminal"]["code"]),
        ("Agent wall time", lambda record: _result(record)["timings"]["agent_ms"]),
        ("Total attempt wall time", lambda record: record["timings"]["total_attempt_ms"]),
        ("Uncached input tokens", lambda record: _result(record)["usage"]["input"]),
        ("Prompt total tokens", lambda record: _result(record)["usage"]["prompt_total"]),
        ("Output tokens", lambda record: _result(record)["usage"]["output"]),
        ("Generation tokens", lambda record: _result(record)["usage"]["generation"]),
        ("All prompt/output tokens", lambda record: _result(record)["usage"]["all_tokens"]),
        ("Reasoning tokens", lambda record: _result(record)["usage"]["reasoning"]),
        ("Cache read tokens", lambda record: _result(record)["usage"]["cache_read"]),
        ("Cache write tokens", lambda record: _result(record)["usage"]["cache_write"]),
        ("Turns", lambda record: _result(record)["counts"]["turns"]),
        ("Model turns", lambda record: _result(record)["counts"]["model_turns"]),
        ("Provider requests", lambda record: _result(record)["counts"]["provider_requests"]),
        ("Tool calls", lambda record: _result(record)["counts"]["tool_calls"]),
        ("Retries", lambda record: _result(record)["counts"]["retries"]),
        ("Compactions", lambda record: _result(record)["counts"]["compactions"]),
        ("Peak RSS", lambda record: record["process"]["peak_rss_bytes"]),
        ("Cost", lambda record: _result(record)["cost"]["total"]),
        ("Cost kind", lambda record: _result(record)["cost"]["kind"]),
        ("Patch SHA-256", lambda record: record["patch_sha256"]),
    ]
    headings = "| Metric | Pi static | Tea static |" if reference is None else "| Metric | Tea static | Tea JIT | Pi static reference |"
    separator = "| --- | ---: | ---: |" if reference is None else "| --- | ---: | ---: | ---: |"
    rows = [headings, separator]
    for label, extract in fields:
        values = [_value(extract(left)), _value(extract(right))]
        if reference is not None:
            values.append(_value(extract(reference)))
        rows.append("| " + label + " | " + " | ".join(values) + " |")
    return rows


def _native_surfaces(pi: dict[str, Any], tea: dict[str, Any]) -> list[str]:
    pi_surface, tea_surface = _result(pi)["surface"], _result(tea)["surface"]
    values = [
        ("System prompt normalized equal", pi_surface["workspace_normalized_system_prompt_sha256"] == tea_surface["workspace_normalized_system_prompt_sha256"]),
        ("System prompt bytes", f"{pi_surface['system_prompt_bytes']} / {tea_surface['system_prompt_bytes']}"),
        ("System prompt hashes", f"{pi_surface['system_prompt_sha256']} / {tea_surface['system_prompt_sha256']}"),
        ("Active tool order equal", pi_surface["active_tools"] == tea_surface["active_tools"]),
        ("Tool-surface hashes", f"{pi_surface['tool_surface_sha256']} / {tea_surface['tool_surface_sha256']}"),
    ]
    return ["| Surface | Pi / Tea |", "| --- | --- |", *[f"| {label} | {_value(value)} |" for label, value in values]]


def static_report(summary: dict[str, Any]) -> str:
    pi, tea = _attempt(summary, "pi-static"), _attempt(summary, "tea-static")
    run = summary["run"]
    lines = [
        f"# Pi vs Tea Static — {run['task_id']}",
        "",
        "On this pinned task and attempt, this report compares the two static harnesses; it is not a broad benchmark claim.",
        "",
        "## Reproducibility",
        "",
        f"- Run ID: `{run['run_id']}`",
        f"- Task manifest: `{run['task_manifest_sha256']}`",
        f"- Baseline commit: `{run['baseline_commit']}`",
        f"- Validator: `{run['validator_sha256']}`",
        f"- Model/provider: `{run['model']}` / `{run['provider']}`",
        f"- Thinking/output ceiling: `{run['thinking_level']}` / `{run['max_output_tokens']}`",
        f"- Shared attempt timeout: `{run['timeout_seconds']}` seconds",
        f"- Run class: `{run.get('run_class', 'unclassified')}`",
        f"- Toolchain manifest: `{run.get('toolchain_manifest_sha256', 'unrecorded')}`",
        f"- Validator dependency lock: `{run.get('validator_dependency_lockfile_sha256', 'unrecorded')}`",
        f"- Condition order: `{', '.join(run['condition_order'])}`",
        "",
        "## Native harness surfaces",
        "",
        *_native_surfaces(pi, tea),
        "",
        "Native prompt and tool differences are measured results of this experiment, not parity gates. See [surface-diff.md](surface-diff.md) for retained, redacted differences.",
        "Each adapter's `surface/wire-requests.json` is the direct final-provider request evidence. The normalized result summary is derived from it and is not a substitute for that retained witness.",
        "",
        "## Results",
        "",
        *_rows(pi, tea),
        "",
        "## Observed comparison",
        "",
        _comparability_note(pi, tea),
        "Provider requests are shown when an adapter exposes an exact count; Pi currently leaves this field null rather than inferring wire requests from turn totals. A provider-routing, request-shape, or observed-route mismatch invalidates a strict efficiency conclusion; native harness differences do not by themselves do so.",
        "",
    ]
    return "\n".join(lines)


def _classification(static: dict[str, Any], jit: dict[str, Any]) -> str:
    static_pass, jit_pass = static["validator"]["passed"], jit["validator"]["passed"]
    decision = _result(jit)["harness"]["decision"]
    if decision == "no-change":
        return "no-change"
    if not static_pass and jit_pass:
        return "positive flip"
    if static_pass and not jit_pass:
        return "regression"
    if static_pass and jit_pass:
        static_result, jit_result = _result(static), _result(jit)
        if (jit_result["usage"]["generation"] < static_result["usage"]["generation"] or jit_result["timings"]["agent_ms"] < static_result["timings"]["agent_ms"]):
            return "efficiency improvement"
        return "efficiency regression"
    return "no observed improvement"


def _comparability_note(*records: dict[str, Any]) -> str:
    failures = [f"{record['baseline_id']}: {_value(_result(record)['terminal']['code'])}" for record in records if _result(record)["terminal"]["status"] != "completed"]
    if failures:
        return "Not comparable as an efficacy result: " + "; ".join(failures) + ". The provider or runtime failed before a completed task attempt."
    return "Interpret results lexicographically: correctness, generation tokens, agent wall time, then cost only when both cost kinds match. No weighted score is computed."


def evolution_report(summary: dict[str, Any]) -> str:
    static, jit, pi = _attempt(summary, "tea-static"), _attempt(summary, "tea-jit"), _attempt(summary, "pi-static")
    harness = _result(jit)["harness"]
    lines = [
        f"# Tea Harness JIT v0 — {summary['run']['task_id']}",
        "",
        "The primary comparison is Tea static versus Tea JIT. Pi static is an external product reference.",
        "",
        "## JIT configuration",
        "",
        "- Candidate / activation / rollover budgets: `1 / 1 / 1`",
        "- Candidate source limit: `16384` bytes",
        "- Capability ceiling: no new ambient capability; no web research; no subagents",
        "- Shell `curl` remains available through `bash`.",
        "",
        "## JIT decision",
        "",
        f"- Decision: `{harness['decision']}`",
        f"- Candidate: `{_value(harness['candidate_id'])}` ({harness['candidate_source_bytes']} bytes)",
        f"- Changed surfaces: `{', '.join(harness['changed_surfaces']) or 'none'}`",
        f"- Base / initial / final snapshot: `{_value(harness['base_snapshot_id'])}` / `{_value(harness['initial_snapshot_id'])}` / `{_value(harness['final_snapshot_id'])}`",
        f"- Candidate validation / rollover: `{_result(jit)['timings']['candidate_validation_ms']}` ms / `{_result(jit)['timings']['rollover_ms']}` ms",
        "",
        "## Results",
        "",
        *_rows(static, jit, pi),
        "",
        "## Classification",
        "",
        _comparability_note(static, jit, pi),
        "",
        f"Observed Tea delta: **{_classification(static, jit)}**. A one-attempt flip is not causal proof.",
        "",
    ]
    return "\n".join(lines)


def write_reports(summary: dict[str, Any], reports: Path) -> tuple[Path, Path, Path]:
    reports.mkdir(parents=True, exist_ok=True)
    static = reports / "static.md"
    evolution = reports / "evolution.md"
    surface = reports / "surface-diff.md"
    static.write_text(static_report(summary), encoding="utf-8")
    evolution.write_text(evolution_report(summary), encoding="utf-8")
    pi, tea = _attempt(summary, "pi-static"), _attempt(summary, "tea-static")
    pi_surface, tea_surface = _result(pi)["surface"], _result(tea)["surface"]
    differences = []
    for name in ("workspace_normalized_system_prompt_sha256", "tool_surface_sha256", "active_tools"):
        if pi_surface[name] != tea_surface[name]:
            differences.append(f"- `{name}` differs: Pi `{pi_surface[name]}`, Tea `{tea_surface[name]}`")
    surface.write_text("# Static surface difference\n\n" + ("\n".join(differences) if differences else "The exported normalized surfaces are equal.") + "\n", encoding="utf-8")
    return static, evolution, surface


def write_static_report(summary: dict[str, Any], reports: Path) -> tuple[Path, Path]:
    """Write the paired static comparison without requiring a JIT attempt."""
    reports.mkdir(parents=True, exist_ok=True)
    static = reports / "static.md"
    surface = reports / "surface-diff.md"
    static.write_text(static_report(summary), encoding="utf-8")
    pi, tea = _attempt(summary, "pi-static"), _attempt(summary, "tea-static")
    pi_surface, tea_surface = _result(pi)["surface"], _result(tea)["surface"]
    differences = []
    for name in ("workspace_normalized_system_prompt_sha256", "tool_surface_sha256", "active_tools"):
        if pi_surface[name] != tea_surface[name]:
            differences.append(f"- `{name}` differs: Pi `{pi_surface[name]}`, Tea `{tea_surface[name]}`")
    surface.write_text("# Static surface difference\n\n" + ("\n".join(differences) if differences else "The exported normalized surfaces are equal.") + "\n", encoding="utf-8")
    return static, surface


def baseline_report(summary: dict[str, Any], baseline: str = "tea-static") -> str:
    """Render one baseline without fabricating a paired comparison."""
    record = _attempt(summary, baseline)
    result = _result(record)
    run = summary["run"]
    fields = [
        ("Validator pass", record["validator"]["passed"]),
        ("Terminal status", result["terminal"]["status"]),
        ("Terminal code", result["terminal"]["code"]),
        ("Agent wall time", result["timings"]["agent_ms"]),
        ("Total attempt wall time", record["timings"]["total_attempt_ms"]),
        ("Generation tokens", result["usage"]["generation"]),
        ("Provider requests", result["counts"]["provider_requests"]),
        ("Tool calls", result["counts"]["tool_calls"]),
        ("Retries", result["counts"]["retries"]),
        ("Patch SHA-256", record["patch_sha256"]),
    ]
    lines = [
        f"# {baseline} baseline — {run['task_id']}",
        "",
        "This is a single-baseline run. It establishes Tea evidence for this pinned task; it is not a paired Pi comparison.",
        "",
        "## Reproducibility",
        "",
        f"- Run ID: `{run['run_id']}`",
        f"- Task manifest: `{run['task_manifest_sha256']}`",
        f"- Baseline commit: `{run['baseline_commit']}`",
        f"- Validator: `{run['validator_sha256']}`",
        f"- Model/provider: `{run['model']}` / `{run['provider']}`",
        f"- Thinking/output ceiling: `{run['thinking_level']}` / `{run['max_output_tokens']}`",
        f"- Attempt timeout: `{run['timeout_seconds']}` seconds",
        f"- Validator dependency lock: `{run.get('validator_dependency_lockfile_sha256', 'unrecorded')}`",
        "",
        "## Result",
        "",
        "| Metric | Value |",
        "| --- | ---: |",
        *[f"| {label} | {_value(value)} |" for label, value in fields],
        "",
        "The complete normalized result is retained in the attempt `record.json`; direct provider request evidence is under the attempt `surface/` directory.",
        "",
    ]
    return "\n".join(lines)


def write_baseline_report(summary: dict[str, Any], reports: Path, *, baseline: str = "tea-static") -> Path:
    """Write the single-baseline report without requiring Pi or JIT attempts."""
    reports.mkdir(parents=True, exist_ok=True)
    path = reports / f"{baseline}.md"
    path.write_text(baseline_report(summary, baseline), encoding="utf-8")
    return path
