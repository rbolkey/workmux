"""Q7: Socket permissions.

Verifies cmux socket has owner-only permissions.
"""

import os
import stat


class TestSocketPermissions:
    """Q7: cmux socket security."""

    def test_socket_permissions(self):
        """Q7: CMUX_SOCKET_PATH has owner-only permissions (mode & 0o077 == 0)."""
        socket_path = os.environ.get("CMUX_SOCKET_PATH")
        assert socket_path, "CMUX_SOCKET_PATH not set"
        assert os.path.exists(socket_path), f"Socket not found: {socket_path}"

        mode = os.stat(socket_path).st_mode
        # Check that group and other have no permissions
        assert mode & 0o077 == 0, (
            f"Socket has non-owner permissions: {stat.filemode(mode)}"
        )
