"""CLI for the explicit, one-task Pi/Tea shootout."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys

from .runner import Config, DEFAULT_MODEL, DEFAULT_THINKING, ShootoutError, plan, run


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
        item.add_argument("--timeout-seconds", type=int, default=900)
        item.add_argument("--repeats", type=int, default=1)
        item.add_argument("--seed", type=int, default=20260823)
        item.add_argument("--cache-root", type=Path, default=Path("/tmp/tea-pi-shootout-cache"))
        item.add_argument("--workspace-root", type=Path, default=Path("/tmp/tea-pi-shootout-workspaces"))
        item.add_argument("--out", type=Path, default=Path("/tmp/tea-pi-shootout"))
        if name == "run":
            item.add_argument("--keep-worktrees", action="store_true")
    sub.add_parser("check", help="run only Python provider-free shootout tests")
    return command


def _config(args: argparse.Namespace) -> Config:
    maximum = None if args.max_output_tokens == "unlimited" else int(args.max_output_tokens)
    return Config(args.task, args.provider, args.model, args.thinking, maximum, args.repeats, args.seed, args.cache_root, args.workspace_root, args.out, timeout_seconds=args.timeout_seconds, keep_worktrees=getattr(args, "keep_worktrees", False))


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        if args.command == "plan":
            print(json.dumps(plan(_config(args)), indent=2, sort_keys=True))
            return 0
        if args.command == "run":
            directory, result = run(_config(args))
            print(f"shootout evidence: {directory}")
            for report in result["reports"]:
                print(f"report: {report}")
            return 0
        if args.command == "check":
            command = [sys.executable, "-m", "unittest", "evals.pi_shootout.test_contract", "evals.pi_shootout.test_report"]
            return subprocess.run(command, cwd=Path(__file__).resolve().parents[2], check=False).returncode
    except (ShootoutError, OSError, ValueError) as error:
        print(f"pi-shootout error: {error}", file=sys.stderr)
        return 2
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
