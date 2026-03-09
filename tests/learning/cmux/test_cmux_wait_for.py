"""Q2: wait-for latch semantics.

Verifies that cmux wait-for signals are latched (persist until consumed)
and that timeout behavior works as expected.
"""

import os
import subprocess
import time


def _unique_channel():
    """Generate a unique wait-for channel name to avoid cross-test interference."""
    return f"wm_test_{os.getpid()}_{time.time_ns()}"


class TestWaitForLatch:
    """Q2: wait-for latch semantics -- signal persists until consumed."""

    def test_wait_for_signal_before_wait(self):
        """Q2: Signal first, then wait returns immediately (latch behavior)."""
        channel = _unique_channel()

        # Signal first (latched)
        subprocess.run(
            ["cmux", "wait-for", "--signal", channel],
            capture_output=True,
            text=True,
            check=True,
        )

        # Wait should return immediately because signal is latched
        result = subprocess.run(
            ["cmux", "wait-for", channel, "--timeout", "3"],
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode == 0, (
            f"wait-for should succeed after signal (latch). stderr: {result.stderr}"
        )

    def test_wait_for_timeout(self):
        """Q2: Wait without signal times out (exit code != 0)."""
        channel = _unique_channel()

        result = subprocess.run(
            ["cmux", "wait-for", channel, "--timeout", "1"],
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode != 0, "wait-for should timeout without signal"

    def test_wait_for_signal_consumed(self):
        """Q2: Signal is consumed by first wait; second wait times out."""
        channel = _unique_channel()

        # Signal
        subprocess.run(
            ["cmux", "wait-for", "--signal", channel],
            capture_output=True,
            check=True,
        )

        # First wait consumes the signal
        result = subprocess.run(
            ["cmux", "wait-for", channel, "--timeout", "3"],
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode == 0, "First wait should consume signal"

        # Second wait should timeout (signal consumed)
        result = subprocess.run(
            ["cmux", "wait-for", channel, "--timeout", "1"],
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode != 0, "Second wait should timeout (signal consumed)"
