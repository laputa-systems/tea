from __future__ import annotations

import _thread
import json
import os
from pathlib import Path
import signal
import sys
import tempfile
import threading
import time
import unittest
from contextlib import redirect_stderr, redirect_stdout
from io import StringIO
from unittest.mock import patch

from .__main__ import main
from .runner import (
    AttemptOutcome,
    ProcessFinalization,
    ProcessOutcome,
    STOP_REQUEST_FILENAME,
    STOP_TARGET_FILENAME,
    _run_process,
    _run_process_with_stop,
    _require_settled_process,
    _split_attempt_outcomes,
    _write_process_finalization,
    _write_exclusion,
    attempt_hard_timeout_seconds,
    run_repeat_lanes,
)


class OperatorStopTest(unittest.TestCase):
    def _target(self, directory: Path, *, eligible: bool = True) -> dict[str, object]:
        target = {
            "schema_version": "tea-pi-shootout-stop-target/v1",
            "attempt_id": "shootout-r1-tea-static",
            "baseline_id": "tea-static",
            "repeat_lane": 1,
            "operator_stop": {"eligible": eligible, "policy": "tea-only-diagnostic-v1" if eligible else "disabled"},
        }
        (directory / STOP_TARGET_FILENAME).write_text(json.dumps(target), encoding="utf-8")
        return target

    def _sleeping_command(self, pid_path: Path) -> list[str]:
        program = (
            "import os, pathlib, time\n"
            f"pathlib.Path({str(pid_path)!r}).write_text(str(os.getpid()))\n"
            "time.sleep(30)\n"
        )
        return [sys.executable, "-c", program]

    def test_malformed_marker_stops_as_infrastructure_not_exclusion(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            target = self._target(directory)
            (directory / STOP_REQUEST_FILENAME).write_text("not json", encoding="utf-8")
            outcome = _run_process_with_stop(
                self._sleeping_command(directory / "pid"),
                cwd=directory,
                environment=os.environ.copy(),
                timeout_seconds=10,
                stop_target=target,
                stop_request_path=directory / STOP_REQUEST_FILENAME,
            )
            self.assertIsNone(outcome.operator_stop)
            self.assertIsNotNone(outcome.stop_protocol_error)
            self.assertFalse(outcome.timed_out)

    def test_controller_stop_rejects_non_diagnostic_target(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            self._target(directory, eligible=False)
            errors = StringIO()
            with redirect_stderr(errors):
                self.assertEqual(
                    main(["stop", "--attempt-dir", str(directory), "--reason", "operator-requested"]),
                    2,
                )
            self.assertIn("Tea-only diagnostic", errors.getvalue())
            self.assertFalse((directory / STOP_REQUEST_FILENAME).exists())

    def test_accepted_stop_writes_exclusion_and_keeps_sibling_outcome(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            target = self._target(directory)
            output = StringIO()
            with redirect_stdout(output):
                self.assertEqual(
                    main(["stop", "--attempt-dir", str(directory), "--reason", "operator-requested"]),
                    0,
                )
            self.assertIn("shootout-r1-tea-static", output.getvalue())
            request = json.loads((directory / STOP_REQUEST_FILENAME).read_text(encoding="utf-8"))
            outcome = _run_process_with_stop(
                self._sleeping_command(directory / "pid"),
                cwd=directory,
                environment=os.environ.copy(),
                timeout_seconds=10,
                stop_target=target,
                stop_request_path=directory / STOP_REQUEST_FILENAME,
            )
            self.assertEqual(outcome.operator_stop, request)
            exclusion = _write_exclusion(
                directory,
                attempt_id="shootout-r1-tea-static",
                baseline="tea-static",
                repeat=1,
                process=outcome,
                patch="partial patch",
                changed_files=["lib/router/index.js"],
            )
            persisted = json.loads((directory / "exclusion.json").read_text(encoding="utf-8"))
            self.assertEqual(persisted, exclusion)
            self.assertEqual(exclusion["kind"], "operator_stopped")
            self.assertEqual(exclusion["stop_request"], request)
            self.assertEqual(exclusion["patch"]["changed_files"], ["lib/router/index.js"])
            first = AttemptOutcome.excluded(exclusion)
            second = AttemptOutcome.completed({"attempt_id": "shootout-r2-tea-static", "baseline_id": "tea-static"})
            lanes = run_repeat_lanes(
                [["tea-static"], ["tea-static"]],
                2,
                lambda repeat, _order, _cancellation: [[first], [second]][repeat],
            )
            attempts, exclusions = _split_attempt_outcomes(lanes)
            self.assertEqual([attempt["attempt_id"] for attempt in attempts], ["shootout-r2-tea-static"])
            self.assertEqual([item["attempt_id"] for item in exclusions], ["shootout-r1-tea-static"])

    @unittest.skipUnless(os.name == "posix", "raw signal semantics are POSIX-specific")
    def test_raw_sigterm_is_not_reclassified_as_operator_stop(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            target = self._target(directory)
            pid_path = directory / "pid"

            def interrupt_child() -> None:
                deadline = time.monotonic() + 5
                while not pid_path.exists() and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertTrue(pid_path.exists())
                os.kill(int(pid_path.read_text(encoding="utf-8")), signal.SIGTERM)

            interrupter = threading.Thread(target=interrupt_child)
            interrupter.start()
            try:
                outcome = _run_process_with_stop(
                    self._sleeping_command(pid_path),
                    cwd=directory,
                    environment=os.environ.copy(),
                    timeout_seconds=10,
                    stop_target=target,
                    stop_request_path=directory / STOP_REQUEST_FILENAME,
                )
            finally:
                interrupter.join(timeout=5)
            self.assertIsNone(outcome.operator_stop)
            self.assertIsNone(outcome.stop_protocol_error)
            self.assertFalse(outcome.timed_out)
            self.assertEqual(outcome.exit_code, -signal.SIGTERM)


class StaticFinalizationGraceTest(unittest.TestCase):
    def test_static_adapters_receive_identical_finalization_grace(self) -> None:
        self.assertEqual(attempt_hard_timeout_seconds("pi-static", 900), 915)
        self.assertEqual(attempt_hard_timeout_seconds("tea-static", 900), 915)
        self.assertEqual(attempt_hard_timeout_seconds("tea-jit", 900), 900)

    def test_diagnostic_zero_remains_an_uncapped_outer_wall_clock(self) -> None:
        self.assertEqual(attempt_hard_timeout_seconds("pi-static", 0), 0)
        self.assertEqual(attempt_hard_timeout_seconds("tea-static", 0), 0)


@unittest.skipUnless(os.name == "posix", "session containment is POSIX-specific")
class ProcessFinalizationTest(unittest.TestCase):
    def _assert_raw_interrupt_stops_term_ignoring_leader(self, *, with_stop_protocol: bool) -> None:
        """A controller interrupt must not return while its adapter leader survives."""
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            pid_path = directory / "leader.pid"
            ready_marker = directory / "leader-ready"
            leader_program = (
                "import os, pathlib, signal, time\n"
                "signal.signal(signal.SIGTERM, lambda *_: None)\n"
                f"pathlib.Path({str(pid_path)!r}).write_text(str(os.getpid()))\n"
                f"pathlib.Path({str(ready_marker)!r}).write_text('ready')\n"
                "time.sleep(60)\n"
            )
            interrupt_sent = threading.Event()

            def interrupt_after_leader_starts() -> None:
                deadline = time.monotonic() + 2
                while not ready_marker.exists() and time.monotonic() < deadline:
                    time.sleep(0.01)
                if ready_marker.exists():
                    interrupt_sent.set()
                    _thread.interrupt_main()

            interrupter = threading.Thread(target=interrupt_after_leader_starts)
            interrupter.start()
            try:
                with patch("evals.pi_shootout.runner.PROCESS_DRAIN_SECONDS", 0.1):
                    with self.assertRaises(KeyboardInterrupt):
                        if with_stop_protocol:
                            _run_process_with_stop(
                                [sys.executable, "-c", leader_program],
                                cwd=directory,
                                environment=os.environ.copy(),
                                timeout_seconds=10,
                                stop_target={},
                                stop_request_path=directory / "no-stop-request.json",
                            )
                        else:
                            _run_process(
                                [sys.executable, "-c", leader_program],
                                cwd=directory,
                                environment=os.environ.copy(),
                                timeout_seconds=10,
                            )
                interrupter.join(timeout=2)
                self.assertTrue(interrupt_sent.is_set(), "the test did not interrupt a live adapter leader")
                self.assertTrue(pid_path.exists(), "the test interrupted before the adapter leader recorded its PID")
                leader_pid = int(pid_path.read_text(encoding="utf-8"))
                deadline = time.monotonic() + 1
                while time.monotonic() < deadline:
                    try:
                        os.kill(leader_pid, 0)
                    except ProcessLookupError:
                        break
                    time.sleep(0.01)
                else:
                    self.fail("a TERM-ignoring adapter leader survived controller interruption")
            finally:
                interrupter.join(timeout=2)
                if pid_path.exists():
                    leader_pid = int(pid_path.read_text(encoding="utf-8"))
                    try:
                        os.kill(leader_pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    try:
                        os.waitpid(leader_pid, 0)
                    except ChildProcessError:
                        pass

    def test_raw_interrupt_forces_term_ignoring_adapter_leader_to_stop(self) -> None:
        self._assert_raw_interrupt_stops_term_ignoring_leader(with_stop_protocol=False)

    def test_raw_interrupt_forces_term_ignoring_adapter_leader_to_stop_with_stop_protocol(self) -> None:
        self._assert_raw_interrupt_stops_term_ignoring_leader(with_stop_protocol=True)

    def test_parallel_raw_interrupt_settles_worker_owned_adapter_leaders(self) -> None:
        """A main-thread Ctrl-C must reach leaders owned by repeat workers."""
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            pid_paths = [directory / "leader-0.pid", directory / "leader-1.pid"]
            ready_paths = [directory / "leader-0.ready", directory / "leader-1.ready"]
            interrupt_sent = threading.Event()

            def one_lane(repeat: int, _order: list[str], cancellation: threading.Event) -> ProcessOutcome:
                leader_program = (
                    "import os, pathlib, signal, time\n"
                    "signal.signal(signal.SIGTERM, lambda *_: None)\n"
                    f"pathlib.Path({str(pid_paths[repeat])!r}).write_text(str(os.getpid()))\n"
                    f"pathlib.Path({str(ready_paths[repeat])!r}).write_text('ready')\n"
                    "time.sleep(60)\n"
                )
                return _run_process(
                    [sys.executable, "-c", leader_program],
                    cwd=directory,
                    environment=os.environ.copy(),
                    timeout_seconds=10,
                    cancellation=cancellation,
                )

            def interrupt_after_all_leaders_start() -> None:
                deadline = time.monotonic() + 2
                while not all(path.exists() for path in ready_paths) and time.monotonic() < deadline:
                    time.sleep(0.01)
                if all(path.exists() for path in ready_paths):
                    interrupt_sent.set()
                    _thread.interrupt_main()

            interrupter = threading.Thread(target=interrupt_after_all_leaders_start)
            interrupter.start()
            started = time.monotonic()
            try:
                with patch("evals.pi_shootout.runner.PROCESS_DRAIN_SECONDS", 0.1):
                    with self.assertRaises(KeyboardInterrupt):
                        run_repeat_lanes([["pi-static"], ["tea-static"]], 2, one_lane)
                interrupter.join(timeout=2)
                self.assertTrue(interrupt_sent.is_set(), "the test did not interrupt live worker-owned leaders")
                self.assertLess(time.monotonic() - started, 2, "parallel interruption waited for the adapter timeout")
                for pid_path in pid_paths:
                    self.assertTrue(pid_path.exists(), "worker leader did not record its PID")
                    leader_pid = int(pid_path.read_text(encoding="utf-8"))
                    deadline = time.monotonic() + 1
                    while time.monotonic() < deadline:
                        try:
                            os.kill(leader_pid, 0)
                        except ProcessLookupError:
                            break
                        time.sleep(0.01)
                    else:
                        self.fail("a worker-owned TERM-ignoring adapter leader survived controller interruption")
            finally:
                interrupter.join(timeout=2)
                for pid_path in pid_paths:
                    if not pid_path.exists():
                        continue
                    leader_pid = int(pid_path.read_text(encoding="utf-8"))
                    try:
                        os.kill(leader_pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    try:
                        os.waitpid(leader_pid, 0)
                    except ChildProcessError:
                        pass

    def test_post_exit_pipe_holder_is_bounded_and_not_settled(self) -> None:
        """An exited leader cannot turn an inherited-pipe holder into success."""
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            child_pid_path = directory / "child.pid"
            child_program = "import time\ntime.sleep(30)\n"
            leader_program = (
                "import pathlib, subprocess, sys\n"
                f"child = subprocess.Popen([sys.executable, '-c', {child_program!r}], start_new_session=True)\n"
                f"pathlib.Path({str(child_pid_path)!r}).write_text(str(child.pid))\n"
                "print('leader stdout', flush=True)\n"
                "print('leader stderr', file=sys.stderr, flush=True)\n"
            )
            started = time.monotonic()
            with patch("evals.pi_shootout.runner.PROCESS_DRAIN_SECONDS", 0.1):
                outcome = _run_process(
                    [sys.executable, "-c", leader_program],
                    cwd=directory,
                    environment=os.environ.copy(),
                    timeout_seconds=5,
                )
            elapsed = time.monotonic() - started
            try:
                self.assertLess(elapsed, 2)
                self.assertEqual(outcome.finalization.status, "post-exit-pipe-open")
                self.assertFalse(outcome.finalization.stdout_complete)
                self.assertFalse(outcome.finalization.stderr_complete)
                self.assertIn("leader stdout", outcome.stdout)
                self.assertIn("leader stderr", outcome.stderr)
            finally:
                if child_pid_path.exists():
                    try:
                        os.kill(int(child_pid_path.read_text(encoding="utf-8")), signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    def test_post_exit_same_session_descendant_is_cleaned_before_return(self) -> None:
        """A nested process group remains attributable after its leader exits."""
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            child_pid_path = directory / "child.pid"
            ready_path = directory / "ready"
            escaped_path = directory / "escaped"
            child_program = (
                "import os, pathlib, time\n"
                "os.setpgrp()\n"
                f"pathlib.Path({str(child_pid_path)!r}).write_text(str(os.getpid()))\n"
                f"pathlib.Path({str(ready_path)!r}).write_text('ready')\n"
                "time.sleep(0.5)\n"
                f"pathlib.Path({str(escaped_path)!r}).write_text('escaped')\n"
                "time.sleep(30)\n"
            )
            leader_program = (
                "import pathlib, subprocess, sys, time\n"
                f"subprocess.Popen([sys.executable, '-c', {child_program!r}])\n"
                f"while not pathlib.Path({str(ready_path)!r}).exists(): time.sleep(0.01)\n"
                "print('leader stdout', flush=True)\n"
            )
            outcome = _run_process(
                [sys.executable, "-c", leader_program],
                cwd=directory,
                environment=os.environ.copy(),
                timeout_seconds=5,
            )
            try:
                self.assertEqual(outcome.finalization.status, "post-exit-descendants-cleaned")
                self.assertTrue(outcome.finalization.session_groups_before_cleanup)
                self.assertEqual(outcome.finalization.session_groups_after_cleanup, ())
                self.assertTrue(outcome.finalization.stdout_complete)
                time.sleep(0.7)
                self.assertFalse(escaped_path.exists(), "same-session descendant survived process finalization")
            finally:
                if child_pid_path.exists():
                    try:
                        os.kill(int(child_pid_path.read_text(encoding="utf-8")), signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    def test_tea_only_stop_runner_also_bounds_post_exit_pipe_holders(self) -> None:
        """The authenticated-stop polling path shares the ordinary finalizer."""
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            child_pid_path = directory / "child.pid"
            child_program = "import time\ntime.sleep(30)\n"
            leader_program = (
                "import pathlib, subprocess, sys\n"
                f"child = subprocess.Popen([sys.executable, '-c', {child_program!r}], start_new_session=True)\n"
                f"pathlib.Path({str(child_pid_path)!r}).write_text(str(child.pid))\n"
            )
            with patch("evals.pi_shootout.runner.PROCESS_DRAIN_SECONDS", 0.1):
                outcome = _run_process_with_stop(
                    [sys.executable, "-c", leader_program],
                    cwd=directory,
                    environment=os.environ.copy(),
                    timeout_seconds=5,
                    stop_target={},
                    stop_request_path=directory / "no-stop-request.json",
                )
            try:
                self.assertEqual(outcome.finalization.status, "post-exit-pipe-open")
                self.assertFalse(outcome.finalization.stdout_complete)
                self.assertFalse(outcome.finalization.stderr_complete)
            finally:
                if child_pid_path.exists():
                    try:
                        os.kill(int(child_pid_path.read_text(encoding="utf-8")), signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    def test_unsettled_finalization_writes_no_output_to_its_sidecar_and_cannot_score(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            process = ProcessOutcome(
                exit_code=0,
                timed_out=False,
                stdout="partial stdout",
                stderr="partial stderr",
                elapsed_ms=1,
                pid=123,
                finalization=ProcessFinalization(
                    status="post-exit-pipe-open",
                    stdout_complete=False,
                    stderr_complete=False,
                    session_groups_before_cleanup=(456,),
                ),
            )
            directory = Path(temporary)
            _write_process_finalization(directory, process)
            sidecar = json.loads((directory / "process-finalization.json").read_text(encoding="utf-8"))
            self.assertEqual(sidecar["status"], "post-exit-pipe-open")
            self.assertEqual(sidecar["session"]["groups_before_cleanup"], [456])
            self.assertNotIn("partial stdout", json.dumps(sidecar, sort_keys=True))
            self.assertNotIn("partial stderr", json.dumps(sidecar, sort_keys=True))
            with self.assertRaisesRegex(RuntimeError, "process finalization=post-exit-pipe-open"):
                _require_settled_process("tea-static", process)


if __name__ == "__main__":
    unittest.main()
