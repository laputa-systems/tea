"""Command line for the deliberately small quality suite."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys

from .coding_cases import CodingCaseError
from .coding_runner import CodingRunError, prepare_cache
from .compaction import CompactionQualityError, run_compaction_quality
from .multiedit import MultiEditQualityError, grade_verified_record, write_multiedit_task
from .multiedit_runner import MultiEditRunError, run_multiedit
from .suite import AdapterError, ContractError, inspect_environment, run_fast, run_rust_allocation_probe


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(prog="python3 -m evals.quality")
    sub = command.add_subparsers(dest="command", required=True)
    fast = sub.add_parser("fast", help="run provider-free strict core fixtures")
    fast.add_argument("--case", action="append", default=[], help="run one named enabled core case (repeatable)")
    fast.add_argument("--out", type=Path, help="persist self-contained artifacts in this directory")
    compaction = sub.add_parser("compaction", help="run the provider-free compaction contract matrix")
    compaction.add_argument("--out", type=Path, required=True, help="write one content-free report per scenario")
    compaction.add_argument(
        "--update-baseline",
        action="store_true",
        help="replace the checked-in contract baseline; requires --reason",
    )
    compaction.add_argument("--reason", help="required audit reason for --update-baseline")
    multiedit = sub.add_parser("multiedit-disabled", help="materialize or grade the hermetic disabled-tool multiedit eval")
    multiedit.add_argument("--out", type=Path, required=True, help="write the public-only task and optional grade report")
    multiedit.add_argument("--record", type=Path, help="runner-produced hidden-validator record to grade")
    multiedit.add_argument("--run", action="store_true", help="run a candidate through the repository-owned isolated runner")
    multiedit.add_argument("--timeout-seconds", type=float, default=30.0, help="per-candidate-phase timeout for --run")
    multiedit.add_argument("--command", nargs=argparse.REMAINDER, help="candidate command for --run; place this option last")
    sub.add_parser("inspect-environment", help="print the explicit core-evaluation surfaces")
    resources = sub.add_parser("resources", help="measure Rust allocations and peak RSS with Rustybench")
    resources.add_argument("--out", type=Path, help="write the JSON resource artifact to this file")
    cache = sub.add_parser("prepare-cache", help="explicitly populate the pinned Express bare-repository cache")
    cache.add_argument("--cache-root", type=Path, required=True, help="explicit cache root; this operation may fetch pinned commits")
    cache.add_argument("--case", action="append", default=[], help="prepare one named coding case (repeatable)")
    return command


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        if args.command == "inspect-environment":
            print(json.dumps(inspect_environment(), indent=2, sort_keys=True))
            return 0
        if args.command == "fast":
            status, summary = run_fast(case_ids=args.case or None, out=args.out)
            print(
                f"quality fast: {summary['matches']}/{summary['case_count']} cases passed; "
                f"strict failures: {len(summary['strict_failures'])}"
            )
            if summary["strict_failures"]:
                print("failed: " + ", ".join(summary["strict_failures"]))
            if args.out:
                print(f"artifacts: {args.out}")
            return status
        if args.command == "compaction":
            status, summary = run_compaction_quality(
                out=args.out,
                update_baseline=args.update_baseline,
                reason=args.reason,
            )
            print(
                f"quality compaction: {summary['passed']}/{summary['scenario_count']} scenarios passed; "
                f"continuation episodes: {summary['continuation_fixtures']['passed']}/"
                f"{summary['continuation_fixtures']['case_count']}; "
                f"offline baseline contract: {summary['baseline']['matched_contract']}"
            )
            print(f"artifacts: {args.out}")
            return status
        if args.command == "multiedit-disabled":
            if args.run and args.record is not None:
                raise MultiEditQualityError("multiedit-disabled accepts either --run or --record, not both")
            if args.run:
                record = run_multiedit(
                    out=args.out,
                    command=args.command or (),
                    timeout_seconds=args.timeout_seconds,
                )
                grade = grade_verified_record(record)
                report = args.out / "grade.json"
                report.write_text(json.dumps(grade, indent=2, sort_keys=True) + "\n", encoding="utf-8")
                print(f"multiedit disabled-tool run: {grade['total_score']}/100; artifacts: {args.out}")
                return 0 if grade["passed"] else 1
            task = write_multiedit_task(args.out)
            if args.record is None:
                print(f"multiedit disabled-tool task: {task}")
                return 0
            try:
                record = json.loads(args.record.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError) as error:
                raise MultiEditQualityError(f"cannot read trusted runner record: {error}") from error
            if not isinstance(record, dict):
                raise MultiEditQualityError("trusted runner record must be a JSON object")
            grade = grade_verified_record(record)
            report = args.out / "grade.json"
            report.write_text(json.dumps(grade, indent=2, sort_keys=True) + "\n", encoding="utf-8")
            print(f"multiedit disabled-tool grade: {grade['total_score']}/100; artifacts: {args.out}")
            return 0 if grade["passed"] else 1
        if args.command == "resources":
            result = run_rust_allocation_probe(args.out)
            print(json.dumps(result, indent=2, sort_keys=True))
            return 0
        if args.command == "prepare-cache":
            result = prepare_cache(cache_root=args.cache_root, case_ids=args.case or None)
            print(json.dumps(result, indent=2, sort_keys=True))
            return 0
    except ContractError as error:
        print(f"quality contract error: {error}", file=sys.stderr)
        return 2
    except AdapterError as error:
        print(f"quality resource error: {error}", file=sys.stderr)
        return 2
    except (CodingRunError, CodingCaseError) as error:
        print(f"quality coding error: {error}", file=sys.stderr)
        return 2
    except CompactionQualityError as error:
        print(f"quality compaction error: {error}", file=sys.stderr)
        return 2
    except (MultiEditQualityError, MultiEditRunError) as error:
        print(f"quality multiedit error: {error}", file=sys.stderr)
        return 2
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
