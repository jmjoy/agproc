//! Process exit codes.
//!
//! These are part of the public contract: the agent skill documents them so an
//! AI agent can branch on the outcome without parsing human text.

/// Success: service ready, already running, stop ok, ps/logs/skills.
pub const OK: i32 = 0;
/// Generic internal error (including "CLI wait timed out").
pub const GENERIC: i32 = 1;
/// Usage error. clap exits with this code itself; the constant documents the
/// contract for the agent skill.
#[allow(dead_code)]
pub const USAGE: i32 = 2;
/// Configuration error: missing or invalid `agproc.toml`.
pub const CONFIG: i32 = 3;
/// `build-cmd` failed (non-zero exit or timeout).
pub const BUILD_FAILED: i32 = 4;
/// `run-cmd` exited before readiness.
pub const RUN_FAILED: i32 = 5;
/// Readiness probe failed.
pub const PROBE_FAILED: i32 = 6;
/// Another start/restart is in progress for this service.
pub const LOCKED: i32 = 7;
/// This start was superseded (stopped or restarted by another invocation).
pub const SUPERSEDED: i32 = 8;
