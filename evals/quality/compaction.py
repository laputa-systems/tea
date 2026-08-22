"""Provider-free compaction quality contract and artifact writer.

The suite is deliberately a contract lane, not a synthetic provider benchmark.
It validates the core transaction tests (including the optional trace adapter),
then writes one content-free report per scenario.  It has no network, model,
credential, or ambient-cache dependency.  Provider canaries are a separate,
explicitly opt-in concern.
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import subprocess
from typing import Any, Iterable

from .checkpoint import LedgerEntry, StructuredCheckpoint, checkpoint_fingerprint
from .continuation import run_continuation_fixtures


ROOT = Path(__file__).resolve().parents[2]
BASELINE = Path(__file__).resolve().parent / "cases" / "compaction" / "baseline.json"
SCHEMA = "tea-compaction-quality/v1"
TOOLCHAIN = "nightly-2026-07-24"


class CompactionQualityError(ValueError):
    """The compaction quality invocation or checked-in baseline is invalid."""


def _scenario_group(prefix: str, names: Iterable[str], test_target: str) -> list[dict[str, str]]:
    return [
        {
            "id": f"{prefix}-{name}",
            "family": prefix,
            "test_target": test_target,
        }
        for name in names
    ]


# These cases name concrete context-pressure and history-transition contracts.
# Their execution is shared by focused Rust tests rather than reimplemented in
# Python, so the quality report cannot silently drift from the core boundary.
SCENARIOS = tuple(
    _scenario_group(
        "threshold-overflow",
        (
            "threshold-before-request",
            "threshold-retained-tail",
            "threshold-tool-pair-boundary",
            "overflow-single-retry",
            "overflow-second-rejected",
            "overflow-valid-usage-checkpoint",
            "overflow-unavailable-compactor",
            "near-budget-minimum-headroom",
        ),
        "automatic_policy",
    )
    + _scenario_group(
        "tool-output",
        (
            "large-result-byte-pressure",
            "large-result-retained-tail",
            "tool-call-result-integrity",
            "tool-error-result-integrity",
            "mixed-small-and-large-results",
            "repeated-large-results",
            "projection-does-not-rewrite-canonical-history",
        ),
        "automatic_policy",
    )
    + _scenario_group(
        "multi-step-state",
        (
            "user-edit-request-follow-up",
            "create-then-inspect",
            "create-then-fix",
            "rename-then-reference",
            "multi-file-state-carryover",
            "earlier-constraint-recovery",
            "assistant-plan-retention",
            "recent-turn-priority",
            "split-turn-user-prefix",
            "tool-boundary-no-partial-pair",
        ),
        "compaction",
    )
    + _scenario_group(
        "cache-layout",
        (
            "system-prompt-stable",
            "tool-definition-stable",
            "tool-order-stable",
            "model-stable",
            "thinking-stable",
            "exact-context-extension",
            "append-only-context",
            "adapter-envelope-domain-change",
            "post-compaction-request-joined",
            "fallback-layout-recorded",
        ),
        "cache_friendliness",
    )
    + _scenario_group(
        "concurrency-cancellation",
        (
            "manual-active-run-refused",
            "manual-cancel-before-provider",
            "automatic-cancel-before-commit",
            "stale-source-cas",
            "observer-steering-stale-source",
            "one-owner-per-run",
            "retry-limit-after-overflow",
        ),
        "compaction",
    )
    + _scenario_group(
        "provider-failure",
        (
            "missing-compactor-unavailable",
            "compactor-failure-non-mutating",
            "invalid-replacement-non-mutating",
            "provider-overflow-typed",
            "provider-timeout-classified",
            "provider-stall-classified",
            "provider-cancellation-classified",
        ),
        "compaction",
    )
    + _scenario_group(
        "checkpoint-semantics",
        (
            "nonempty-checkpoint",
            "whitespace-checkpoint-rejected",
            "checkpoint-no-tool-call",
            "checkpoint-no-tool-result",
            "strict-size-reduction",
            "minimum-working-headroom",
            "retained-suffix-exact",
            "source-generation-matches",
            "valid-message-ids",
            "valid-tool-pairs",
            "commit-atomicity",
            "failure-preserves-history",
        ),
        "automatic_policy",
    )
    + _scenario_group(
        "compatibility-determinism",
        (
            "lifecycle-id-stable",
            "lifecycle-terminal-once",
            "trace-content-free",
            "trace-v0-records-unchanged",
            "trace-v1-additive",
            "request-observation-same-stream",
            "measurement-pure",
            "offline-no-network",
            "deterministic-replay",
        ),
        "trace",
    )
)


def _canonical(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def _digest(value: Any) -> str:
    return hashlib.sha256(_canonical(value).encode("utf-8")).hexdigest()


def _load_baseline() -> dict[str, Any]:
    try:
        baseline = json.loads(BASELINE.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise CompactionQualityError(f"cannot read baseline {BASELINE}: {error}") from error
    if baseline.get("schema_version") != SCHEMA:
        raise CompactionQualityError("compaction baseline has an unsupported schema_version")
    manifest = baseline.get("baseline_manifest")
    if not isinstance(manifest, dict) or not all(
        isinstance(manifest.get(key), str) and manifest[key]
        for key in (
            "tea_commit",
            "strategy_id",
            "summary_prompt_fingerprint",
            "compaction_policy",
            "estimator_version",
            "fixture_corpus_version",
        )
    ):
        raise CompactionQualityError("compaction baseline manifest is incomplete")
    if not isinstance(manifest.get("strategy_schema_version"), int) or not isinstance(
        manifest.get("trace_schema_version"), int
    ):
        raise CompactionQualityError("compaction baseline manifest has invalid schema versions")
    if not isinstance(manifest.get("expected_exceptions"), list):
        raise CompactionQualityError("compaction baseline manifest omits expected exceptions")
    if baseline.get("scenario_ids") != [scenario["id"] for scenario in SCENARIOS]:
        raise CompactionQualityError("compaction baseline scenario IDs differ from the checked-in contract")
    return baseline


def _run_target(target: str) -> dict[str, Any]:
    command = [
        "rustup",
        "run",
        TOOLCHAIN,
        "cargo",
        "test",
        "-p",
        "tea-core",
    ]
    if target == "trace":
        command.extend(("--features", "trace", "--lib", "trace::tests"))
    else:
        command.extend(("--test", target))
    completed = subprocess.run(
        command,
        cwd=ROOT,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    return {
        "command": command,
        "exit_code": completed.returncode,
        "stdout_sha256": hashlib.sha256(completed.stdout.encode("utf-8")).hexdigest(),
        "stderr_sha256": hashlib.sha256(completed.stderr.encode("utf-8")).hexdigest(),
    }


def _structured_candidate_report() -> dict[str, Any]:
    checkpoint = StructuredCheckpoint.empty()
    for generation in range(1, 6):
        checkpoint = checkpoint.merge(
            {
                "Goal": ["continue the active task"],
                "Current Checkpoint": [f"generation {generation}"],
                "Verification": [f"fixture-{generation} passed"],
            },
            [LedgerEntry("test", f"fixture-{generation}", "/workspace", "passed", generation=generation)],
        )
    rendered = checkpoint.render()
    return {
        "strategy_id": "structured_checkpoint_v1",
        "schema_version": 1,
        "changed_dimension": "checkpoint schema and deterministic workspace ledger",
        "hard_invariants": "passed_provider_free",
        "checkpoint_generation": checkpoint.generation,
        "checkpoint_bytes": len(rendered.encode("utf-8")),
        "ledger_entries": len(checkpoint.ledger.entries),
        "checkpoint_fingerprint": checkpoint_fingerprint(rendered),
        "provider_quality": "unresolved; experimental strategy is not runtime-default",
    }


def _markdown_report(summary: dict[str, Any]) -> str:
    """Render a compact human-readable run report beside the JSON artifact."""

    continuation = summary["continuation_fixtures"]
    lines = [
        "# Compaction quality report",
        "",
        f"- Rust-contract coverage rows: {summary['passed']}/{summary['scenario_count']}",
        f"- Executed Rust targets: {summary['executed_target_count']}",
        f"- Deterministic continuation episodes: {continuation['passed']}/{continuation['case_count']}",
        f"- Provider cache accounting: {summary['metrics']['provider_cache_accounting']}",
        f"- Default decision: {summary['default_decision']}",
        "",
        "## Continuation episodes",
        "",
        "| Episode | Critical facts | Obsolete facts | Headroom | Next compaction | Repeated read bytes | Duplicate tools |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for case in continuation["cases"]:
        metrics = case["metrics"]
        lines.append(
            "| {id} | {survived}/{total} | {obsolete} | {headroom} | {next_compaction} | {reread} | {duplicates} |".format(
                id=case["id"],
                survived=metrics["critical_facts_survived"],
                total=metrics["critical_facts_total"],
                obsolete=metrics["obsolete_facts_present"],
                headroom=metrics["headroom_tokens"],
                next_compaction=metrics["requests_until_next_compaction"],
                reread=metrics["repeated_unchanged_read_bytes"],
                duplicates=metrics["duplicate_tool_calls"],
            )
        )
    lines.extend(("", "## Strategy status", ""))
    for name, strategy in summary["strategy_comparison"].items():
        lines.append(f"- `{strategy['strategy_id']}` ({name}): {strategy.get('status', strategy.get('hard_invariants'))}")
    lines.extend(
        (
            "",
            "Deterministic checkpoints validate the transaction and evaluator contracts only. "
            "They do not establish provider-generated semantic quality or a provider cache hit.",
            "",
        )
    )
    return "\n".join(lines)


def run_compaction_quality(*, out: Path, update_baseline: bool = False, reason: str | None = None) -> tuple[int, dict[str, Any]]:
    """Execute the offline compaction matrix and write reproducible artifacts."""

    if update_baseline and not reason:
        raise CompactionQualityError("--update-baseline requires a non-empty --reason")
    baseline = _load_baseline()
    target_results = {target: _run_target(target) for target in sorted({item["test_target"] for item in SCENARIOS})}
    continuation = run_continuation_fixtures()
    scenario_reports: list[dict[str, Any]] = []
    for scenario in SCENARIOS:
        target = target_results[scenario["test_target"]]
        scenario_reports.append(
            {
                **scenario,
                "passed": target["exit_code"] == 0,
                "evidence": {
                    "test_target": scenario["test_target"],
                    "command_sha256": _digest(target["command"]),
                    "stdout_sha256": target["stdout_sha256"],
                    "stderr_sha256": target["stderr_sha256"],
                },
            }
        )
    summary = {
        "schema_version": SCHEMA,
        "network": False,
        "provider": None,
        "scenario_count": len(scenario_reports),
        "executed_target_count": len(target_results),
        "fixture_case_count": continuation["case_count"],
        "passed": sum(1 for report in scenario_reports if report["passed"]),
        "failed": [report["id"] for report in scenario_reports if not report["passed"]],
        "scenario_ids": [report["id"] for report in scenario_reports],
        "targets": target_results,
        "metrics": {
            "scenario_rows": len(scenario_reports),
            "fixture_compaction_episodes": continuation["case_count"],
            "provider_cache_accounting": "unavailable_offline",
            "prefix_proxy": "not_a_cache_hit_claim",
            "model_free_pruning": "not_promoted_without_a_dominant-tool-result-baseline",
        },
        "default_decision": "cache_replay_summary_v0 remains the default; no candidate is promoted by offline evidence alone",
        "strategy_comparison": {
            "baseline": {
                "strategy_id": "cache_replay_summary_v0",
                "schema_version": 0,
                "status": "default",
                "provider_quality": "one free-model canary committed; later free retries were rejected before compaction; semantic quality unresolved",
            },
            "structured_checkpoint": _structured_candidate_report(),
            "incremental_checkpoint": {
                "strategy_id": "incremental_checkpoint_update_v1",
                "schema_version": 1,
                "changed_dimension": "previous checkpoint plus newly discarded span",
                "status": "runtime-bound experimental candidate; no promotion",
                "provider_quality": "unresolved",
            },
            "tool_free_replay": {
                "strategy_id": "tool_free_replay_summary_v1",
                "schema_version": 1,
                "changed_dimension": "tool definitions omitted from otherwise exact replay",
                "status": "runtime-bound provider compatibility candidate; no promotion",
                "provider_quality": "unresolved",
            },
        },
        "continuation_fixtures": continuation,
    }
    summary["report_sha256"] = _digest({key: value for key, value in summary.items() if key != "report_sha256"})
    if update_baseline:
        baseline = {
            "schema_version": SCHEMA,
            "scenario_ids": summary["scenario_ids"],
            "baseline_reason": reason,
            "baseline_report_sha256": summary["report_sha256"],
            # Strategy/prompt provenance is a reviewed checked-in contract. An
            # update command never silently invents a new manifest value.
            "baseline_manifest": baseline["baseline_manifest"],
        }
        BASELINE.write_text(json.dumps(baseline, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    baseline_match = baseline.get("scenario_ids") == summary["scenario_ids"]
    summary["baseline"] = {
        "matched_contract": baseline_match,
        "baseline_report_sha256": baseline.get("baseline_report_sha256"),
        "baseline_reason": baseline.get("baseline_reason"),
        "manifest": baseline["baseline_manifest"],
    }
    out.mkdir(parents=True, exist_ok=True)
    (out / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    (out / "summary.md").write_text(_markdown_report(summary), encoding="utf-8")
    for report in scenario_reports:
        (out / f"{report['id']}.json").write_text(
            json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    continuation_out = out / "continuation"
    continuation_out.mkdir(exist_ok=True)
    for report in continuation["cases"]:
        (continuation_out / f"{report['id']}.json").write_text(
            json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    return (0 if not summary["failed"] and not continuation["failed"] and baseline_match else 1), summary
