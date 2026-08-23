#!/usr/bin/env python3
"""Deterministic, provider-free adapter used to exercise the v1 controller contract.

This is intentionally a tiny task adapter, not an agent implementation.  It proves that a
manifest can pass explicit task/workspace/result/identity arguments through the controller and
that controller-owned oracles score the resulting workspace.  It never reads credentials or
opens a network connection.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import sys
from typing import Any


RESULT_SCHEMA = "tea-coding-eval-result/v1"


def read_object(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"{path} must contain an object")
    return value


def write_workspace_file(workspace: Path, relative: str, content: str) -> None:
    destination = (workspace / relative).resolve()
    try:
        destination.relative_to(workspace.resolve())
    except ValueError as exc:
        raise ValueError(f"mock adapter path escapes workspace: {relative!r}") from exc
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(content, encoding="utf-8")


INTERVALS = '''\
def merge_intervals(intervals: list[tuple[int, int]]) -> list[tuple[int, int]]:
    ordered = sorted(intervals)
    merged: list[tuple[int, int]] = []
    for start, end in ordered:
        if start > end:
            raise ValueError("interval start exceeds end")
        if merged and start <= merged[-1][1] + 1:
            merged[-1] = (merged[-1][0], max(merged[-1][1], end))
        else:
            merged.append((start, end))
    return merged
'''

INTERVAL_TESTS = '''\
from intervals import merge_intervals


assert merge_intervals([(5, 7), (1, 2), (3, 4)]) == [(1, 7)]
assert merge_intervals([(1, 10), (2, 6), (8, 12)]) == [(1, 12)]
assert merge_intervals([(1, 2), (3, 4)]) == [(1, 4)]
assert merge_intervals([(1, 2), (4, 5)]) == [(1, 2), (4, 5)]
assert merge_intervals([(-10, -8), (-5, -1)]) == [(-10, -8), (-5, -1)]
assert merge_intervals([]) == []
original = [(5, 7), (1, 2)]
merge_intervals(original)
assert original == [(5, 7), (1, 2)]
try:
    merge_intervals([(2, 1)])
except ValueError:
    pass
else:
    raise AssertionError("invalid interval did not raise")
'''


def result(
    *, attempt_id: str, baseline_id: str, status: str, final_text: str,
    turns: int, tool_calls: int, detail: str | None = None,
) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "schema_version": RESULT_SCHEMA,
        "attempt_id": attempt_id,
        "baseline_id": baseline_id,
        "terminal": {"status": status},
        "final_text": final_text,
        "turns": turns,
        "tool_calls": tool_calls,
        "usage": {"input": 0, "output": 0, "cache_read": 0, "cache_write": 0},
        "trace": [],
    }
    if detail:
        payload["terminal"]["detail"] = detail
    return payload


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--task-json", required=True, type=Path)
    parser.add_argument("--workspace", required=True, type=Path)
    parser.add_argument("--capabilities-json", required=True, type=Path)
    parser.add_argument("--result-json", required=True, type=Path)
    parser.add_argument("--attempt-id", required=True)
    parser.add_argument("--baseline-id", required=True)
    args = parser.parse_args(argv)

    task = read_object(args.task_json)
    capabilities = json.loads(args.capabilities_json.read_text(encoding="utf-8"))
    if capabilities != task.get("capabilities"):
        raise ValueError("capability manifest does not match task")
    task_id = task.get("task_id")
    if task_id == "ready-v1":
        payload = result(
            attempt_id=args.attempt_id, baseline_id=args.baseline_id,
            status="completed", final_text="READY", turns=1, tool_calls=0,
        )
    elif task_id == "interval-merge-v1":
        write_workspace_file(args.workspace, "intervals.py", INTERVALS)
        write_workspace_file(args.workspace, "test_intervals.py", INTERVAL_TESTS)
        try:
            completed = subprocess.run(
                [
                    sys.executable,
                    "-I",
                    "-c",
                    "import runpy,sys; sys.path.insert(0, '.'); runpy.run_path('test_intervals.py', run_name='__main__')",
                ],
                cwd=args.workspace, capture_output=True, text=True,
                timeout=int(task["timeout_seconds"]), check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            payload = result(
                attempt_id=args.attempt_id, baseline_id=args.baseline_id,
                status="failed", final_text="", turns=2, tool_calls=2, detail=str(exc),
            )
        else:
            if completed.returncode == 0:
                payload = result(
                    attempt_id=args.attempt_id, baseline_id=args.baseline_id,
                    status="completed", final_text="Tests passed.", turns=2, tool_calls=2,
                )
            else:
                detail = (completed.stdout + "\n" + completed.stderr).strip()[-2_000:]
                payload = result(
                    attempt_id=args.attempt_id, baseline_id=args.baseline_id,
                    status="failed", final_text="", turns=2, tool_calls=2, detail=detail,
                )
    else:
        raise ValueError(f"mock adapter does not implement task {task_id!r}")

    args.result_json.write_text(json.dumps(payload, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, json.JSONDecodeError) as exc:
        print(f"mock adapter error: {exc}", file=sys.stderr)
        raise SystemExit(2) from exc
