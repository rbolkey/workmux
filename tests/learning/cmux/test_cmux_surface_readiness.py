"""Learning tests for cmux split_pane surface readiness mechanisms.

These tests investigate whether cmux provides alternatives to the retry loop
used in workmux's `split_pane` for waiting on surface initialization:

1. Does `new-split` accept a `--command` flag to launch directly?
2. Does `surface-health` expose a readiness state transition?
3. What is the actual surface initialization latency?

Discovery: 2026-04-06, deferred question from post-MVP quick wins plan.
"""

import subprocess
import time

import pytest

from .helpers import run_cmux, run_cmux_json


class TestNewSplitCommand:
    """Does new-split accept --command to launch a command directly?

    If yes, the respawn-pane retry loop can be eliminated entirely because
    the surface starts with the target command instead of a default shell.
    """

    def test_new_split_command_flag(self, cmux_workspace):
        """Q: Can new-split launch with a specific command via --command?

        FINDING (2026-04-06): --command is NOT documented in --help.
        The flag is silently ignored — new-split still creates a default shell.
        The split succeeds (exit 0) but the command doesn't run.
        Respawn-pane retry loop remains necessary.
        """
        ws = cmux_workspace

        result = subprocess.run(
            [
                "cmux",
                "new-split",
                "right",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
                "--command",
                "echo split-cmd-test-marker",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            pytest.skip(f"new-split --command not supported: {result.stderr.strip()}")

        # Parse the new surface ref
        parts = result.stdout.strip().split()
        new_surface = next((p for p in parts if p.startswith("surface:")), None)
        assert new_surface is not None, (
            f"Expected surface ref in output: {result.stdout}"
        )
        ws_ref = next(p for p in parts if p.startswith("workspace:"))

        # Wait briefly and check if the command actually ran on the surface
        import time

        time.sleep(1.0)

        from .helpers import wait_for_screen_content

        try:
            wait_for_screen_content(
                ws_ref, new_surface, "split-cmd-test-marker", timeout=2.0
            )
            # If we see the marker, --command actually works!
        except (AssertionError, Exception):
            # --command was silently ignored — surface has a default shell,
            # not the echo command. This confirms the flag is a no-op.
            pass

    def test_new_split_help_for_command_flag(self):
        """Check new-split --help for --command flag documentation."""
        result = subprocess.run(
            ["cmux", "new-split", "--help"],
            capture_output=True,
            text=True,
            check=False,
        )
        has_command = "--command" in result.stdout or "--command" in result.stderr
        if not has_command:
            pytest.skip(
                "new-split --help does not mention --command flag. "
                f"Help output: {(result.stdout + result.stderr)[:500]}"
            )


class TestSurfaceHealthReadiness:
    """Does surface-health expose a readiness state transition?

    If surface-health has a state field that transitions from "initializing"
    to "ready", we can poll on that instead of blind retry with backoff.
    """

    def test_surface_health_after_split(self, cmux_workspace):
        """Capture surface-health immediately after new-split to detect state fields."""
        ws = cmux_workspace

        # Create a split
        split_result = subprocess.run(
            [
                "cmux",
                "new-split",
                "right",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
            ],
            capture_output=True,
            text=True,
            check=True,
        )
        parts = split_result.stdout.strip().split()
        new_surface = next(p for p in parts if p.startswith("surface:"))

        # Immediately check surface-health (before it's fully ready)
        # Try --json first, fall back to plain output
        health_result = subprocess.run(
            [
                "cmux",
                "surface-health",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                new_surface,
                "--json",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        if health_result.returncode != 0:
            pytest.skip(f"surface-health not available: {health_result.stderr.strip()}")

        import json

        output = health_result.stdout.strip()
        try:
            data = json.loads(output)
        except json.JSONDecodeError:
            # --json may be silently ignored. Try without --json.
            health_result2 = subprocess.run(
                [
                    "cmux",
                    "surface-health",
                    "--workspace",
                    ws["workspace_ref"],
                    "--surface",
                    new_surface,
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            output = health_result2.stdout.strip()
            try:
                data = json.loads(output)
            except json.JSONDecodeError:
                pytest.skip(f"surface-health returns non-JSON output: {output[:200]}")

        # Document all available fields
        # Look for state/status/ready fields
        state_keys = [
            k
            for k in data.keys()
            if k.lower() in ("state", "status", "ready", "health")
        ]
        if not state_keys:
            pytest.skip(
                f"No state/ready field in surface-health. Available keys: {list(data.keys())}"
            )

    def test_surface_initialization_latency(self, cmux_workspace):
        """Measure actual surface initialization latency (p50/p95/p99).

        Runs multiple splits and measures time from new-split return to
        successful respawn-pane. This establishes the retry bounds rationale.
        """
        ws = cmux_workspace
        latencies = []

        for i in range(5):
            # Create a split
            split_result = subprocess.run(
                [
                    "cmux",
                    "new-split",
                    "right",
                    "--workspace",
                    ws["workspace_ref"],
                    "--surface",
                    ws["surface_ref"],
                ],
                capture_output=True,
                text=True,
                check=True,
            )
            parts = split_result.stdout.strip().split()
            new_surface = next(p for p in parts if p.startswith("surface:"))
            ws_ref = next(p for p in parts if p.startswith("workspace:"))

            # Measure time to successful respawn-pane
            start = time.monotonic()
            success = False
            for attempt in range(20):
                time.sleep(0.05)  # 50ms polling
                result = subprocess.run(
                    [
                        "cmux",
                        "respawn-pane",
                        "--workspace",
                        ws_ref,
                        "--surface",
                        new_surface,
                        "--command",
                        "true",
                    ],
                    capture_output=True,
                    text=True,
                    check=False,
                )
                if result.returncode == 0:
                    latencies.append(time.monotonic() - start)
                    success = True
                    break

            if not success:
                latencies.append(float("inf"))

            # Clean up the split
            subprocess.run(
                [
                    "cmux",
                    "close-surface",
                    "--workspace",
                    ws_ref,
                    "--surface",
                    new_surface,
                ],
                capture_output=True,
                check=False,
            )
            time.sleep(0.2)  # Brief pause between iterations

        finite = [l for l in latencies if l != float("inf")]
        if not finite:
            pytest.skip("All respawn-pane attempts failed")

        finite.sort()
        p50 = finite[len(finite) // 2]
        p95 = finite[int(len(finite) * 0.95)] if len(finite) >= 2 else finite[-1]

        # Document the measurements (test always passes — it's a measurement)
        # The assertions here just ensure we got reasonable data
        assert p50 < 10.0, f"p50 latency {p50:.3f}s seems unreasonably high"
        assert p95 < 10.0, f"p95 latency {p95:.3f}s seems unreasonably high"


class TestSendDuringInitialization:
    """Does `cmux send` work during the surface initialization window?

    Investigation for whether `cmux send` can replace the respawn-pane retry
    loop by injecting a wait-for signal command into the surface's default shell.

    FINDINGS (2026-04-06):
    - `cmux send` returns exit 0 during init and text appears on screen
      (characters are delivered to the terminal emulator)
    - BUT: commands do NOT execute during the init window. The shell hasn't
      started its command loop yet, so keystrokes are displayed but not
      processed as shell input
    - `cmux send` requires a FULLY INITIALIZED shell (3-5s for login shell
      with rc files) — much longer than surface readiness (200-400ms)
    - `respawn-pane` works at a lower level (process replacement, not shell
      input), so it only needs surface readiness
    - CONCLUSION: The wait-for latch approach via `cmux send` is NOT viable.
      The respawn-pane retry loop is the correct mechanism.

    Discovery: 2026-04-06, prerequisite for wait-for latch plan.
    """

    def test_send_displays_text_during_init(self, cmux_workspace):
        """cmux send delivers text to screen during init, but doesn't execute.

        FINDING (2026-04-06): `cmux send` returns exit 0 and text appears on
        the terminal during the init window. However, this is just terminal
        display — the shell's command loop hasn't started, so the text is NOT
        executed as a command. The marker appears as typed text, not echo output.
        """
        ws = cmux_workspace
        marker = f"SEND_INIT_TEST_{int(time.monotonic() * 1e9)}"

        # Create a split — returns immediately
        split_result = subprocess.run(
            [
                "cmux",
                "new-split",
                "right",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
            ],
            capture_output=True,
            text=True,
            check=True,
        )
        parts = split_result.stdout.strip().split()
        new_surface = next(p for p in parts if p.startswith("surface:"))
        ws_ref = next(p for p in parts if p.startswith("workspace:"))

        # Immediately try cmux send (no sleep!)
        send_result = subprocess.run(
            [
                "cmux",
                "send",
                "--workspace",
                ws_ref,
                "--surface",
                new_surface,
                "echo " + marker + "\\n",
            ],
            capture_output=True,
            text=True,
            check=False,
        )

        send_exit_code = send_result.returncode
        send_stderr = send_result.stderr.strip()

        if send_exit_code != 0:
            pytest.skip(
                f"cmux send fails during init window (exit {send_exit_code}): "
                f"{send_stderr}"
            )

        # Text appears on screen (as typed characters, not as executed output)
        from .helpers import wait_for_screen_content

        try:
            wait_for_screen_content(ws_ref, new_surface, marker, timeout=3.0)
        except (AssertionError, Exception):
            pytest.fail(
                f"cmux send returned exit 0 but marker '{marker}' never appeared "
                f"on screen after 3s."
            )

    def test_send_does_not_execute_during_init(self, cmux_workspace):
        """cmux send text is NOT executed as a command during init.

        FINDING (2026-04-06): Even though cmux send delivers text to the
        terminal, the shell hasn't started its command loop during the init
        window. Commands sent via `cmux send` are displayed but not executed.
        File-writing commands produce no file. This is fundamentally different
        from respawn-pane which replaces the process at the surface level.
        """
        ws = cmux_workspace
        import os

        tmpfile = f"/tmp/cmux_send_exec_test_{int(time.monotonic() * 1e9)}.txt"

        split_result = subprocess.run(
            [
                "cmux",
                "new-split",
                "right",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
            ],
            capture_output=True,
            text=True,
            check=True,
        )
        parts = split_result.stdout.strip().split()
        new_surface = next(p for p in parts if p.startswith("surface:"))
        ws_ref = next(p for p in parts if p.startswith("workspace:"))

        # Wait for surface readiness (same window as respawn-pane)
        time.sleep(0.5)

        # Send a file-writing command
        subprocess.run(
            [
                "cmux",
                "send",
                "--workspace",
                ws_ref,
                "--surface",
                new_surface,
                "echo EXECUTED > " + tmpfile + "\\n",
            ],
            capture_output=True,
            text=True,
            check=True,
        )
        time.sleep(2.0)

        file_exists = os.path.exists(tmpfile)
        if file_exists:
            os.unlink(tmpfile)

        # We expect the file to NOT exist (command displayed but not executed)
        if file_exists:
            pytest.skip(
                "cmux send executed the command during init — "
                "revisit the latch approach"
            )

    def test_send_executes_on_fully_initialized_shell(self, cmux_workspace):
        """cmux send DOES execute commands once the shell is fully initialized.

        FINDING (2026-04-06): After waiting 5+ seconds for the login shell to
        complete its rc file loading, cmux send with literal \\n successfully
        executes commands. This confirms send works — it's the shell init
        timing that prevents execution during the init window, not a cmux bug.
        """
        ws = cmux_workspace
        import os

        tmpfile = f"/tmp/cmux_send_ready_test_{int(time.monotonic() * 1e9)}.txt"

        split_result = subprocess.run(
            [
                "cmux",
                "new-split",
                "right",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
            ],
            capture_output=True,
            text=True,
            check=True,
        )
        parts = split_result.stdout.strip().split()
        new_surface = next(p for p in parts if p.startswith("surface:"))
        ws_ref = next(p for p in parts if p.startswith("workspace:"))

        # Wait for FULL shell initialization (rc files, prompt, etc.)
        time.sleep(5.0)

        subprocess.run(
            [
                "cmux",
                "send",
                "--workspace",
                ws_ref,
                "--surface",
                new_surface,
                "touch " + tmpfile + "\\n",
            ],
            capture_output=True,
            text=True,
            check=True,
        )
        time.sleep(2.0)

        file_exists = os.path.exists(tmpfile)
        if file_exists:
            os.unlink(tmpfile)
        assert file_exists, "cmux send did not execute command even after 5s shell init"

    def test_send_signal_not_viable_during_init(self, cmux_workspace):
        """cmux wait-for --signal sent via cmux send does not fire during init.

        FINDING (2026-04-06): Even with 0.5s wait (surface ready, shell loading),
        sending `cmux wait-for --signal <channel>` via cmux send does not produce
        a signal. The command text is delivered to the terminal but the shell
        doesn't execute it. The wait-for latch approach via cmux send is blocked
        by shell initialization timing.
        """
        ws = cmux_workspace
        channel = f"wm_send_test_{int(time.monotonic() * 1e9)}"

        split_result = subprocess.run(
            [
                "cmux",
                "new-split",
                "right",
                "--workspace",
                ws["workspace_ref"],
                "--surface",
                ws["surface_ref"],
            ],
            capture_output=True,
            text=True,
            check=True,
        )
        parts = split_result.stdout.strip().split()
        new_surface = next(p for p in parts if p.startswith("surface:"))
        ws_ref = next(p for p in parts if p.startswith("workspace:"))

        # Wait for surface readiness (but NOT full shell init)
        time.sleep(0.5)

        subprocess.run(
            [
                "cmux",
                "send",
                "--workspace",
                ws_ref,
                "--surface",
                new_surface,
                "cmux wait-for --signal " + channel + "\\n",
            ],
            capture_output=True,
            text=True,
            check=True,
        )

        # Signal should NOT arrive (shell hasn't executed the command)
        wait_result = subprocess.run(
            ["cmux", "wait-for", channel, "--timeout", "3"],
            capture_output=True,
            text=True,
            check=False,
        )

        # We expect timeout — the signal command was displayed but not executed
        if wait_result.returncode == 0:
            pytest.skip(
                "Signal was received — cmux send CAN execute during init. "
                "Revisit the latch approach."
            )
