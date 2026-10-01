#!/usr/bin/env python3
"""Measure an idle mock-provider `tea` TUI with an isolated process environment.

This resource probe intentionally admits only Tea's built-in `mock` provider. It
does not load provider credentials, send a prompt, or run an evaluation. Supply
the same release binary and sampling arguments for the archived baseline and the
working tree; compare the JSON `maximum_rss_kib` values outside this repository.

A headful `tea` may run as a terminal relay plus a session runtime process.
`maximum_rss_kib` is therefore the combined resident size of the started
process and all of its descendants at each sample; `process_count` and the
per-sample `processes` breakdown show what was summed. `--in-process` sets
`TEA_IN_PROCESS=1` to measure the single-process mode.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import pty
import shutil
import signal
import subprocess
import sys
import tempfile
import time


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    command.add_argument("--executable", type=Path, required=True)
    command.add_argument(
        "--scratch",
        type=Path,
        required=True,
        help="existing caller-owned directory used only for this measurement",
    )
    command.add_argument("--out", type=Path, required=True)
    command.add_argument("--samples", type=int, default=5)
    command.add_argument("--sample-delay-seconds", type=float, default=0.2)
    command.add_argument("--settle-seconds", type=float, default=1.0)
    command.add_argument(
        "--in-process",
        action="store_true",
        help="keep the interactive session in one process (TEA_IN_PROCESS=1)",
    )
    return command


def process_environment(home: Path, in_process: bool) -> dict[str, str]:
    environment = {
        "HOME": str(home),
        "TERM": "xterm-256color",
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
    }
    if in_process:
        environment["TEA_IN_PROCESS"] = "1"
    for name in ("LANG", "LC_ALL", "TZ"):
        if value := os.environ.get(name):
            environment[name] = value
    return environment


def rss_kib(pid: int) -> int:
    completed = subprocess.run(
        ["ps", "-o", "rss=", "-p", str(pid)],
        check=True,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    fields = completed.stdout.split()
    if len(fields) != 1 or not fields[0].isdigit():
        raise RuntimeError("the idle process has no readable RSS sample")
    return int(fields[0])


def descendants(pid: int) -> list[int]:
    """The process and every descendant, by walking the parent table."""
    completed = subprocess.run(
        ["ps", "-A", "-o", "pid=", "-o", "ppid="],
        check=True,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    children: dict[int, list[int]] = {}
    for line in completed.stdout.splitlines():
        fields = line.split()
        if len(fields) == 2 and fields[0].isdigit() and fields[1].isdigit():
            children.setdefault(int(fields[1]), []).append(int(fields[0]))
    found = [pid]
    index = 0
    while index < len(found):
        found.extend(children.get(found[index], []))
        index += 1
    return found


def combined_rss_kib(pid: int) -> dict[int, int]:
    sizes = {}
    for member in descendants(pid):
        try:
            sizes[member] = rss_kib(member)
        except (RuntimeError, subprocess.CalledProcessError):
            continue
    return sizes


def stop(process: subprocess.Popen[bytes], master: int) -> None:
    if process.poll() is None:
        try:
            os.write(master, b"\x03")
        except OSError:
            pass
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.send_signal(signal.SIGTERM)
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=3)
    os.close(master)


def run(arguments: argparse.Namespace) -> dict[str, object]:
    if arguments.samples <= 0 or arguments.sample_delay_seconds < 0 or arguments.settle_seconds < 0:
        raise ValueError("sampling values must be positive (or zero only for delays)")
    if not arguments.executable.is_file() or not os.access(arguments.executable, os.X_OK):
        raise ValueError("--executable must name an existing executable file")
    if not arguments.scratch.is_dir():
        raise ValueError("--scratch must name an existing caller-owned directory")
    if arguments.out.exists():
        raise ValueError("refusing to overwrite --out")
    if not arguments.out.parent.is_dir():
        raise ValueError("--out parent must already exist")

    root = Path(tempfile.mkdtemp(prefix="tea-idle-rss-", dir=arguments.scratch))
    home = root / "home"
    tea_home = root / "tea-home"
    workspace = root / "workspace"
    home.mkdir()
    tea_home.mkdir()
    workspace.mkdir()
    master, slave = pty.openpty()
    process: subprocess.Popen[bytes] | None = None
    try:
        process = subprocess.Popen(
            [
                str(arguments.executable),
                "--tea-home",
                str(tea_home),
                "--cwd",
                str(workspace),
                "--provider",
                "mock",
            ],
            cwd=workspace,
            env=process_environment(home, arguments.in_process),
            stdin=slave,
            stdout=slave,
            stderr=slave,
            start_new_session=True,
        )
        os.close(slave)
        time.sleep(arguments.settle_seconds)
        if process.poll() is not None:
            raise RuntimeError("the mock-only idle TUI exited before sampling")
        samples = []
        breakdowns = []
        for sample in range(arguments.samples):
            sizes = combined_rss_kib(process.pid)
            samples.append(sum(sizes.values()))
            breakdowns.append(sorted(sizes.values()))
            if sample + 1 < arguments.samples:
                time.sleep(arguments.sample_delay_seconds)
        return {
            "schema_version": "tea-idle-rss/v2",
            "provider": "mock",
            "prompt_sent": False,
            "in_process": arguments.in_process,
            "sample_count": len(samples),
            "process_count": max(len(sizes) for sizes in breakdowns),
            "processes": breakdowns,
            "rss_kib": samples,
            "maximum_rss_kib": max(samples),
        }
    finally:
        if process is None:
            os.close(master)
            os.close(slave)
        else:
            stop(process, master)
        shutil.rmtree(root, ignore_errors=True)


def main() -> int:
    arguments = parser().parse_args()
    try:
        result = run(arguments)
        arguments.out.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as error:
        print(f"idle RSS measurement failed: {error}", file=sys.stderr)
        return 2
    print(f"idle RSS measurement wrote {arguments.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
