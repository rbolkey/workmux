//! Learning tests for the SSH transport.
//!
//! These pin down the system `ssh` behaviours the SSH transport relies on:
//! long-lived stdio for the control channel, shell-startup noise before the
//! sentinel hello line (ADR 0001), TTY exec, drops, ControlMaster reuse and
//! the error texts a user sees. They shell out to the system `ssh`, as the
//! transport will, and become its contract tests.
//!
//! Env-gated: they run only with `WORKMUX_TEST_SSH=1`. By default each test
//! starts its own user-mode `sshd` on a free localhost port, with a temporary
//! host key, client key and `ssh_config`, so `~/.ssh` and the system sshd are
//! never touched. Set `WORKMUX_TEST_SSH_TARGET=<host>` (and optionally
//! `WORKMUX_TEST_SSH_CONFIG=<file>`) to run against a real remote through the
//! user's own SSH config instead; tests that need control of the server's
//! configuration are skipped there.
//!
//! ```sh
//! WORKMUX_TEST_SSH=1 cargo test --bin workmux ssh_learning -- --test-threads=4
//! ```

mod control_master;
mod drops;
mod errors;
pub(crate) mod harness;
mod long_lived_stdio;
mod stdout_pollution;
mod tty_exec;
