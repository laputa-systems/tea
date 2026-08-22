"""Command line for the deliberately small quality suite."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys

from .coding_cases import CodingCaseError
from .coding_runner import CodingRunError, prepare_cache, run_coding_cases
from .compaction import CompactionQualityError, run_compaction_quality
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
    sub.add_parser("inspect-environment", help="print the explicit core-evaluation surfaces")
    resources = sub.add_parser("resources", help="measure Rust allocations and peak RSS with Rustybench")
    resources.add_argument("--out", type=Path, help="write the JSON resource artifact to this file")
    cache = sub.add_parser("prepare-cache", help="explicitly populate the pinned Express bare-repository cache")
    cache.add_argument("--cache-root", type=Path, required=True, help="explicit cache root; this operation may fetch pinned commits")
    cache.add_argument("--case", action="append", default=[], help="prepare one named coding case (repeatable)")
    coding = sub.add_parser("coding", help="run opt-in Rust coding evaluations against the three Express cases")
    coding.add_argument("--allow-provider", action="store_true", help="required: adapters make provider requests")
    coding.add_argument("--model", required=True, help="explicit OpenRouter model identifier")
    coding.add_argument("--env-file", type=Path, required=True, help="explicit .env file sourced only by the live adapter boundary")
    coding.add_argument("--cache-root", type=Path, required=True, help="pre-populated bare-repository/dependency cache root")
    coding.add_argument("--workspace-root", type=Path, required=True, help="explicit parent for disposable clean worktrees")
    coding.add_argument("--out", type=Path, required=True, help="persistent artifact directory")
    coding.add_argument("--validator", choices=("fast", "full"), default="fast", help="validator tier after each adapter")
    coding.add_argument("--case", action="append", default=[], help="run one named coding case (repeatable)")
    full = sub.add_parser("full", help="run provider-free core fixtures then the opt-in coding audit validator")
    full.add_argument("--allow-provider", action="store_true", help="required: adapters make provider requests")
    full.add_argument("--model", required=True, help="explicit OpenRouter model identifier")
    full.add_argument("--env-file", type=Path, required=True, help="explicit .env file sourced only by the live adapter boundary")
    full.add_argument("--cache-root", type=Path, required=True, help="pre-populated bare-repository/dependency cache root")
    full.add_argument("--workspace-root", type=Path, required=True, help="explicit parent for disposable clean worktrees")
    full.add_argument("--out", type=Path, required=True, help="persistent artifact directory")
    full.add_argument("--case", action="append", default=[], help="run one named coding case (repeatable)")
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
        if args.command == "resources":
            result = run_rust_allocation_probe(args.out)
            print(json.dumps(result, indent=2, sort_keys=True))
            return 0
        if args.command == "prepare-cache":
            result = prepare_cache(cache_root=args.cache_root, case_ids=args.case or None)
            print(json.dumps(result, indent=2, sort_keys=True))
            return 0
        if args.command == "coding":
            if not args.allow_provider:
                raise CodingRunError("coding evaluation requires --allow-provider before creating a worktree")
            status, summary = run_coding_cases(
                model=args.model,
                cache_root=args.cache_root,
                workspace_root=args.workspace_root,
                out=args.out,
                validator=args.validator,
                env_file=args.env_file,
                case_ids=args.case or None,
            )
            print(f"quality coding: {summary['passed']}/{summary['case_count']} cases passed ({args.validator} validator)")
            return status
        if args.command == "full":
            if not args.allow_provider:
                raise CodingRunError("full evaluation requires --allow-provider before creating a worktree")
            args.out.mkdir(parents=True, exist_ok=True)
            core_status, core_summary = run_fast(out=args.out / "core")
            coding_status, coding_summary = run_coding_cases(
                model=args.model,
                cache_root=args.cache_root,
                workspace_root=args.workspace_root,
                out=args.out / "coding",
                validator="full",
                env_file=args.env_file,
                case_ids=args.case or None,
            )
            summary = {"schema_version": "tea-quality-full-run/v1", "core": core_summary, "coding": coding_summary}
            (args.out / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
            print(
                f"quality full: core {core_summary['matches']}/{core_summary['case_count']} passed; "
                f"coding {coding_summary['passed']}/{coding_summary['case_count']} passed"
            )
            return 0 if core_status == 0 and coding_status == 0 else 1
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
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
