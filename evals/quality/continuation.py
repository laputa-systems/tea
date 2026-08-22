"""Deterministic compaction continuation fixtures and raw metric calculations.

This module validates a compact, checked-in fixture corpus without a provider
or model. It is intentionally not an LLM-summary evaluator: fixtures provide
the checkpoint facts and continuation operations explicitly, then this runner
proves the merge, obsolete-state, ledger, and rework contracts around them.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any


FIXTURE_SCHEMA = "tea-compaction-continuation/v1"
FIXTURES = Path(__file__).resolve().parent / "cases" / "compaction" / "continuation.json"


class ContinuationFixtureError(ValueError):
    """A checked-in deterministic continuation fixture is malformed."""


@dataclass(frozen=True)
class Fact:
    """One asserted durable fact with a stable ID and retention class."""

    id: str
    value: str
    critical: bool


def _load() -> dict[str, Any]:
    try:
        value = json.loads(FIXTURES.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ContinuationFixtureError(f"cannot read continuation fixtures: {error}") from error
    if value.get("schema_version") != FIXTURE_SCHEMA:
        raise ContinuationFixtureError("continuation fixture schema is unsupported")
    cases = value.get("cases")
    if not isinstance(cases, list) or not cases:
        raise ContinuationFixtureError("continuation fixtures must contain non-empty cases")
    return value


def _facts(items: Any) -> list[Fact]:
    if not isinstance(items, list):
        raise ContinuationFixtureError("generation facts must be an array")
    facts: list[Fact] = []
    for value in items:
        if not isinstance(value, dict) or not all(isinstance(value.get(key), str) for key in ("id", "value")):
            raise ContinuationFixtureError("fact requires string id and value")
        facts.append(Fact(value["id"], value["value"], bool(value.get("critical", False))))
    return facts


def _merge_generations(case: dict[str, Any]) -> tuple[dict[str, Fact], set[str], int]:
    active: dict[str, Fact] = {}
    obsolete: set[str] = set()
    generations = case.get("generations")
    if not isinstance(generations, list) or not generations:
        raise ContinuationFixtureError(f"{case.get('id')}: generations must be non-empty")
    for generation in generations:
        if not isinstance(generation, dict):
            raise ContinuationFixtureError(f"{case.get('id')}: generation must be an object")
        for fact_id in generation.get("obsolete", []):
            if not isinstance(fact_id, str):
                raise ContinuationFixtureError(f"{case.get('id')}: obsolete fact ID must be a string")
            active.pop(fact_id, None)
            obsolete.add(fact_id)
        for fact in _facts(generation.get("facts", [])):
            active[fact.id] = fact
            obsolete.discard(fact.id)
    return active, obsolete, len(generations)


def _continuation_metrics(operations: Any) -> dict[str, int]:
    if not isinstance(operations, list):
        raise ContinuationFixtureError("continuation operations must be an array")
    seen: set[tuple[str, str, int]] = set()
    file_generation: dict[str, int] = {}
    workspace_generation = 0
    duplicate_tool_calls = 0
    repeated_unchanged_read_bytes = 0
    repeated_failed_approaches = 0
    first_productive: int | None = None
    tool_calls = 0
    for index, operation in enumerate(operations, start=1):
        if not isinstance(operation, dict):
            raise ContinuationFixtureError("continuation operation must be an object")
        kind = operation.get("kind")
        target = operation.get("target")
        if not isinstance(kind, str) or not isinstance(target, str):
            raise ContinuationFixtureError("continuation operation requires kind and target")
        tool_calls += 1
        generation = workspace_generation if kind == "command" else file_generation.get(target, 0)
        signature = (kind, target, generation)
        duplicate = signature in seen
        if duplicate:
            duplicate_tool_calls += 1
        seen.add(signature)
        if kind == "read" and duplicate:
            repeated_unchanged_read_bytes += int(operation.get("bytes", 0))
        if kind == "command" and duplicate and operation.get("status") == "failed":
            repeated_failed_approaches += 1
        if kind in {"write", "edit", "delete"}:
            file_generation[target] = generation + 1
            workspace_generation += 1
        if first_productive is None and bool(operation.get("productive", False)):
            first_productive = index
    return {
        "tool_calls": tool_calls,
        "duplicate_tool_calls": duplicate_tool_calls,
        "repeated_unchanged_read_bytes": repeated_unchanged_read_bytes,
        "repeated_failed_approaches": repeated_failed_approaches,
        "tool_calls_until_productive_action": first_productive or 0,
    }


def _checkpoint_bytes(active: dict[str, Fact], generations: int, ledger_entries: int) -> int:
    lines = [f"<!-- tea-checkpoint:v1 generation={generations} -->"]
    lines.extend(f"- {fact.id}: {fact.value}" for fact in sorted(active.values(), key=lambda fact: fact.id))
    lines.append(f"- workspace-ledger-entries: {ledger_entries}")
    return len("\n".join(lines).encode("utf-8"))


def run_continuation_fixtures() -> dict[str, Any]:
    """Evaluate every checked-in deterministic episode once with exact metrics."""

    fixture = _load()
    reports: list[dict[str, Any]] = []
    for case in fixture["cases"]:
        if not isinstance(case, dict) or not isinstance(case.get("id"), str):
            raise ContinuationFixtureError("each continuation case requires an ID")
        active, obsolete, generations = _merge_generations(case)
        required = set(case.get("required_fact_ids", []))
        forbidden = set(case.get("forbidden_fact_ids", []))
        if not all(isinstance(value, str) for value in required | forbidden):
            raise ContinuationFixtureError(f"{case['id']}: fact IDs must be strings")
        critical_total = sum(1 for fact in active.values() if fact.critical)
        critical_survived = sum(1 for fact_id in required if active.get(fact_id, Fact("", "", False)).critical)
        missing_required = sorted(required - set(active))
        resurrected = sorted(forbidden & set(active))
        expected = case.get("expected", {})
        if not isinstance(expected, dict):
            raise ContinuationFixtureError(f"{case['id']}: expected must be an object")
        metrics = _continuation_metrics(case.get("continuation", []))
        ledger_entries = int(case.get("ledger_entries", 0))
        metrics.update(
            {
                "critical_facts_total": critical_total,
                "critical_facts_survived": critical_survived,
                "required_facts_missing": len(missing_required),
                "obsolete_facts_present": len(resurrected),
                "contradictions": len(resurrected),
                "checkpoint_generation": generations,
                "checkpoint_bytes": _checkpoint_bytes(active, generations, ledger_entries),
                "ledger_entries": ledger_entries,
                "headroom_tokens": int(expected.get("headroom_tokens", 0)),
                "requests_until_next_compaction": int(expected.get("requests_until_next_compaction", 0)),
                "immediate_recompaction": bool(expected.get("immediate_recompaction", False)),
            }
        )
        hard_pass = not missing_required and not resurrected and not metrics["immediate_recompaction"]
        # Both comparison rows use the same asserted checkpoint facts. This proves evaluator and
        # lifecycle contracts only; it intentionally says nothing about a provider's summary skill.
        reports.append(
            {
                "id": case["id"],
                "hard_pass": hard_pass,
                "missing_required_fact_ids": missing_required,
                "resurrected_obsolete_fact_ids": resurrected,
                "metrics": metrics,
                "strategy_comparison": {
                    "baseline_fixture": {
                        "strategy_id": "cache_replay_summary_v0",
                        "evidence": "deterministic_checkpoint_fixture",
                        "hard_pass": hard_pass,
                    },
                    "structured_fixture": {
                        "strategy_id": "structured_checkpoint_v1",
                        "evidence": "deterministic_checkpoint_fixture",
                        "hard_pass": hard_pass,
                    },
                    "incremental_fixture": {
                        "strategy_id": "incremental_checkpoint_update_v1",
                        "evidence": "deterministic_checkpoint_fixture",
                        "hard_pass": hard_pass,
                    },
                },
            }
        )
    return {
        "schema_version": FIXTURE_SCHEMA,
        "evidence": "deterministic fixture; not provider-generated semantic quality",
        "case_count": len(reports),
        "passed": sum(1 for report in reports if report["hard_pass"]),
        "failed": [report["id"] for report in reports if not report["hard_pass"]],
        "cases": reports,
    }
