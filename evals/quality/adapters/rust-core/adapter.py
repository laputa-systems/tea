#!/usr/bin/env python3
"""JSON adapter for the Rust tea-fixtures executable.

The adapter owns process setup only.  The Rust executable remains the
implementation of fixture parsing, execution, and canonical normalization.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
from typing import Any


PROTOCOL = "tea-quality-adapter/v0"
ADAPTER = "rust-core"
TOOLCHAIN = "nightly-2026-07-24"


class ContractError(Exception):
    """An invalid adapter request or unavailable pinned runner."""


def fail(message: str) -> int:
    print(f"{ADAPTER} adapter: {message}", file=sys.stderr)
    return 2


def repository_root() -> Path:
    # adapter.py lives at evals/quality/adapters/rust-core/.
    return Path(__file__).resolve().parents[4]


def read_request() -> dict[str, Any]:
    raw = sys.stdin.read()
    if not raw.strip():
        raise ContractError("stdin must contain one JSON request object")
    try:
        value = json.loads(raw)
    except json.JSONDecodeError as error:
        raise ContractError(f"stdin is not valid JSON: {error.msg}") from error
    if not isinstance(value, dict):
        raise ContractError("request must be a JSON object")
    protocol = value.get("protocol", PROTOCOL)
    if protocol != PROTOCOL:
        raise ContractError(f"unsupported protocol {protocol!r}")
    if value.get("operation", "run") != "run":
        raise ContractError("operation must be 'run'")
    fixture = value.get("fixture")
    if not isinstance(fixture, str) or not fixture:
        raise ContractError("request.fixture must be a non-empty path string")
    return {"fixture": fixture}


def explicit_fixture(root: Path, value: str) -> Path:
    path = Path(value)
    if not path.is_absolute():
        path = root / path
    path = path.resolve()
    if not path.is_file():
        raise ContractError(f"fixture is not a regular file: {path}")
    return path


def check_toolchain(root: Path) -> None:
    toolchain = root / "rust-toolchain.toml"
    if not toolchain.is_file():
        raise ContractError(f"missing Rust toolchain pin: {toolchain}")
    marker = 'channel = "' + TOOLCHAIN + '"'
    if marker not in toolchain.read_text(encoding="utf-8"):
        raise ContractError(f"rust-toolchain.toml does not pin {TOOLCHAIN}")


def run_runner(root: Path, fixture: Path) -> tuple[int, Any]:
    runner = root / "crates" / "tea-core" / "src" / "bin" / "tea-fixtures.rs"
    if not runner.is_file():
        raise ContractError(f"missing Rust fixture runner source: {runner}")
    check_toolchain(root)
    completed = subprocess.run(
        [
            "rustup",
            "run",
            TOOLCHAIN,
            "cargo",
            "run",
            "--quiet",
            "-p",
            "tea-core",
            "--features",
            "fixture-runner",
            "--bin",
            "tea-fixtures",
            "--",
            str(fixture),
        ],
        cwd=root,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if completed.stderr:
        print(completed.stderr, file=sys.stderr, end="")
    if completed.returncode not in (0, 1):
        raise ContractError(f"Rust fixture runner exited with status {completed.returncode}")
    try:
        result = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise ContractError(f"Rust fixture runner did not emit one JSON result: {error.msg}") from error
    if not isinstance(result, dict):
        raise ContractError("Rust fixture runner result must be a JSON object")
    return completed.returncode, result


def main() -> int:
    try:
        request = read_request()
        root = repository_root()
        input_fixture = explicit_fixture(root, request["fixture"])
        status, result = run_runner(root, input_fixture)
        response = {
            "protocol": PROTOCOL,
            "adapter": ADAPTER,
            "metadata": {
                "crate": "tea-core",
                "runner": "crates/tea-core/src/bin/tea-fixtures.rs",
                "toolchain": TOOLCHAIN,
                "fixture_sha256": hashlib.sha256(input_fixture.read_bytes()).hexdigest(),
                "tui": False,
                "ambient_discovery": False,
                "network": False,
            },
            "runner_status": status,
            "result": result,
        }
        print(json.dumps(response, separators=(",", ":")))
        return status
    except (ContractError, OSError, ValueError) as error:
        return fail(str(error))


if __name__ == "__main__":
    raise SystemExit(main())
