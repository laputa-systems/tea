"""CLI for the explicit, one-task Pi/Tea shootout."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys

from .compare import ComparisonError, compare_run, write_comparison
from .runner import (
    OPERATOR_STOP_REASONS,
    Config,
    DEFAULT_MODEL,
    DEFAULT_THINKING,
    DEFAULT_TIMEOUT_SECONDS,
    TASK_TIMEOUT_SECONDS,
    ShootoutError,
    _write_stop_request,
    plan,
    run,
)


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(prog="python3 -m evals.pi_shootout")
    sub = command.add_subparsers(dest="command", required=True)
    for name in ("plan", "run"):
        item = sub.add_parser(name)
        item.add_argument("--task", default="express-3936-medium")
        item.add_argument("--provider", default="openrouter")
        item.add_argument("--model", default=DEFAULT_MODEL)
        item.add_argument("--thinking", default=DEFAULT_THINKING)
        item.add_argument("--max-output-tokens", default="unlimited", help="must be unlimited for the fixed v0 experiment")
        item.add_argument(
            "--timeout-seconds",
            type=int,
            default=None,
            help=(
                "outer wall-clock limit; defaults to the task policy "
                f"({DEFAULT_TIMEOUT_SECONDS}s medium, {TASK_TIMEOUT_SECONDS['express-4205-hard']}s hard; 0 is uncapped diagnostic mode)"
            ),
        )
        item.add_argument("--repeats", type=int, default=1)
        item.add_argument(
            "--parallel-repeats",
            type=int,
            help="independent repeat lanes to run concurrently; defaults to --repeats",
        )
        item.add_argument("--seed", type=int, default=20260823)
        item.add_argument("--cache-root", type=Path, default=Path("/tmp/tea-pi-shootout-cache"))
        item.add_argument("--workspace-root", type=Path, default=Path("/tmp/tea-pi-shootout-workspaces"))
        item.add_argument("--out", type=Path, default=Path("/tmp/tea-pi-shootout"))
        if name == "run":
            item.add_argument("--keep-worktrees", action="store_true")
        item.add_argument("--static-only", action="store_true", help="run only pi-static and tea-static")
        item.add_argument("--tea-only", action="store_true", help="run only the tea-static baseline and write single-baseline evidence")
        item.add_argument(
            "--tool-child-sandbox",
            choices=("none", "macos-seatbelt-v1", "macos-seatbelt-v2"),
            default="none",
            help="Tea-only diagnostic shell-child isolation; never valid for a paired comparison",
        )
        item.add_argument(
            "--edit-recovery-projection",
            choices=("none", "canonical-v1"),
            default="none",
            help="Tea-only diagnostic invalid-edit recovery hint; never valid for a paired comparison",
        )
        item.add_argument(
            "--pre-edit-tool-gate",
            choices=("none", "direct-edit-v1", "source-local-v1"),
            default="none",
            help="fresh static paired policy; source-local-v1 also limits pre-edit read/edit to versioned task targets",
        )
        item.add_argument(
            "--post-edit-validation-gate",
            choices=("none", "unmasked-evidence-v1"),
            default="none",
            help="fresh paired source-local workflow condition requiring visible direct bash evidence after each successful edit",
        )
        item.add_argument(
            "--static-prompt-profile",
            choices=("builtin-v1", "no-history-v1", "prefix-guard-v1", "prefix-guard-focused-v1"),
            default="builtin-v1",
            help="explicit Tea static prompt profile; prefix-guard profiles are Tea-only diagnostic evidence",
        )
    sub.add_parser("check", help="run only Python provider-free shootout tests")
    stop = sub.add_parser("stop", help="request a controller-recognized stop for one Tea-only diagnostic attempt")
    stop.add_argument("--attempt-dir", type=Path, required=True)
    stop.add_argument("--reason", choices=OPERATOR_STOP_REASONS, required=True)
    compare = sub.add_parser("compare", help="compare persisted pi-static and tea-static evidence")
    compare.add_argument("--run-dir", type=Path, required=True)
    compare.add_argument("--output", type=Path)
    compare.add_argument("--markdown", type=Path)
    return command


def _config(args: argparse.Namespace) -> Config:
    maximum = None if args.max_output_tokens == "unlimited" else int(args.max_output_tokens)
    timeout_seconds = args.timeout_seconds
    if timeout_seconds is None:
        timeout_seconds = TASK_TIMEOUT_SECONDS.get(args.task, DEFAULT_TIMEOUT_SECONDS)
    return Config(
        args.task,
        args.provider,
        args.model,
        args.thinking,
        maximum,
        args.repeats,
        args.seed,
        args.cache_root,
        args.workspace_root,
        args.out,
        timeout_seconds=timeout_seconds,
        keep_worktrees=getattr(args, "keep_worktrees", False),
        static_only=getattr(args, "static_only", False),
        tea_only=getattr(args, "tea_only", False),
        parallel_repeats=getattr(args, "parallel_repeats", None),
        tool_child_sandbox=getattr(args, "tool_child_sandbox", "none"),
        edit_recovery_projection=getattr(args, "edit_recovery_projection", "none"),
        pre_edit_tool_gate=getattr(args, "pre_edit_tool_gate", "none"),
        post_edit_validation_gate=getattr(args, "post_edit_validation_gate", "none"),
        static_prompt_profile=getattr(args, "static_prompt_profile", "builtin-v1"),
    )


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        if args.command == "stop":
            request = _write_stop_request(args.attempt_dir.resolve(), args.reason)
            print(f"operator stop requested for {request['attempt_id']}")
            return 0
        if args.command == "plan":
            print(json.dumps(plan(_config(args)), indent=2, sort_keys=True))
            return 0
        if args.command == "run":
            directory, result = run(
                _config(args),
                on_run_started=lambda run_directory: print(f"shootout evidence: {run_directory}", flush=True),
            )
            for report in result["reports"]:
                print(f"report: {report}")
            return 0
        if args.command == "compare":
            analysis = compare_run(args.run_dir.resolve())
            output = args.output or args.run_dir / "reports" / "comparison.json"
            markdown = args.markdown or args.run_dir / "reports" / "comparison.md"
            write_comparison(analysis, output, markdown)
            print(f"comparison: {output}")
            print(f"comparison report: {markdown}")
            return 0
        if args.command == "check":
            command = [sys.executable, "-m", "unittest", "evals.pi_shootout.test_contract", "evals.pi_shootout.test_runner", "evals.pi_shootout.test_report", "evals.pi_shootout.test_compare"]
            return subprocess.run(command, cwd=Path(__file__).resolve().parents[2], check=False).returncode
    except (ComparisonError, ShootoutError, OSError, ValueError) as error:
        print(f"pi-shootout error: {error}", file=sys.stderr)
        return 2
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
