# Learning tests for cmux CLI behavior.
# Run from inside a cmux terminal: pytest tests/learning/cmux/ -v
#
# These tests verify assumptions from the cmux spike investigation.
# Each test file covers one or more spike questions:
#
#   Q1: Workspace creation & surface discovery  → test_cmux_workspace_lifecycle.py
#   Q2: wait-for latch semantics                → test_cmux_wait_for.py
#   Q3: Cross-workspace targeting               → test_cmux_surface_targeting.py
#   Q4: JSON output schemas                     → test_cmux_json_schemas.py
#   Q5: new-split return format                 → test_cmux_surface_targeting.py
#   Q6: surface-health & sidebar-state gaps     → test_cmux_json_schemas.py
#   Q7: Socket permissions                      → test_cmux_socket.py
#   Q8: Send escape handling & buffer paste     → test_cmux_send.py
#
# Classes group tests by spike question for readable output -- this is a
# deliberate departure from the bare-function style used in the main test suite.
