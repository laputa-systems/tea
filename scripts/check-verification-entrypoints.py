#!/usr/bin/env python3
"""Reject default evaluation entry points that could initiate inference."""

from __future__ import annotations

from pathlib import Path
import sys


ROOT = Path(__file__).resolve().parent.parent


def require_absent(path: Path, forbidden: tuple[str, ...]) -> list[str]:
    source = path.read_text(encoding="utf-8")
    return [f"{path.relative_to(ROOT)} contains retired live entry point {value!r}" for value in forbidden if value in source]


def main() -> int:
    failures = [
        *require_absent(ROOT / "Makefile", ("run-rust-live.sh", "evals.quality coding", "evals.quality full")),
        *require_absent(
            ROOT / "evals" / "quality" / "__main__.py",
            ('sub.add_parser("coding"', 'sub.add_parser("full"', "run_coding_cases("),
        ),
    ]
    live_shell = (ROOT / "evals" / "run-rust-live.sh").read_text(encoding="utf-8")
    if "No provider request was made." not in live_shell or "exec cargo run" in live_shell:
        failures.append("evals/run-rust-live.sh must fail closed without running Cargo")
    coding_runner = (ROOT / "evals" / "quality" / "coding_runner.py").read_text(encoding="utf-8")
    if "subprocess." in coding_runner or "_retired_run_coding_cases" in coding_runner:
        failures.append("evals/quality/coding_runner.py must not retain a provider transport implementation")
    if failures:
        print("\n".join(f"verification entrypoint audit: {failure}" for failure in failures), file=sys.stderr)
        return 1
    print("verification entrypoint audit: default Make and quality CLI routes are provider-free")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
