//! Runtime state: one JSON document per service, written atomically by the
//! runner and read by every other command.
//!
//! A record is trusted only while the runner that wrote it is still alive, and
//! only while its `starttime` still matches (which is what makes PID reuse
//! harmless). A record whose runner vanished mid-flight is reported as `stale`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::paths::Project;
use crate::procinfo;
use crate::util::{now_unix_ms, write_atomic};

pub const SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// Running `build-cmd`.
    Building,
    /// `run-cmd` is up, the probe has not settled yet.
    Starting,
    /// The probe passed, the service is up.
    Running,
    /// `build-cmd` exited non-zero (or timed out).
    BuildFailed,
    /// `run-cmd` exited before the probe passed.
    RunFailed,
    /// `run-cmd` exited after having been ready.
    Exited,
    /// The probe hit its failure threshold; the child was stopped.
    ProbeFailed,
    /// Stopped by `agproc stop` (or while building).
    Stopped,
    /// The runner disappeared without recording an outcome (`kill -9`).
    Stale,
}

impl Phase {
    /// Canonical machine-readable name (also the `--json` value).
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Building => "building",
            Phase::Starting => "starting",
            Phase::Running => "running",
            Phase::BuildFailed => "build-failed",
            Phase::RunFailed => "run-failed",
            Phase::Exited => "exited",
            Phase::ProbeFailed => "probe-failed",
            Phase::Stopped => "stopped",
            Phase::Stale => "stale",
        }
    }

    /// Human-facing wording.
    pub fn display(self) -> &'static str {
        match self {
            Phase::Building => "building",
            Phase::Starting => "starting",
            Phase::Running => "running",
            Phase::BuildFailed => "build failed",
            Phase::RunFailed => "run failed",
            Phase::Exited => "running failed",
            Phase::ProbeFailed => "probe failed",
            Phase::Stopped => "stopped",
            Phase::Stale => "stale",
        }
    }

    /// Terminal phase for a `start` caller waiting for an outcome.
    pub fn settled(self) -> bool {
        matches!(
            self,
            Phase::Running
                | Phase::BuildFailed
                | Phase::RunFailed
                | Phase::Exited
                | Phase::ProbeFailed
                | Phase::Stopped
                | Phase::Stale
        )
    }

    /// Exit code a waiting `start` should report for this phase.
    pub fn start_exit_code(self) -> i32 {
        match self {
            Phase::Running => crate::exit::OK,
            Phase::BuildFailed => crate::exit::BUILD_FAILED,
            Phase::RunFailed | Phase::Exited | Phase::Stale => crate::exit::RUN_FAILED,
            Phase::ProbeFailed => crate::exit::PROBE_FAILED,
            Phase::Stopped => crate::exit::SUPERSEDED,
            Phase::Building | Phase::Starting => crate::exit::GENERIC,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offsets {
    pub stdout: u64,
    pub stderr: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProbeState {
    pub kind: String,
    pub target: String,
    pub attempts: u32,
    pub last_error: Option<String>,
    /// Listener that already owned the port before `run-cmd` started, when any.
    pub foreign_port_pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct State {
    pub schema: u32,
    pub service: String,
    pub generation: u64,
    pub phase: Phase,
    pub runner_pid: u32,
    pub runner_start_ticks: u64,
    pub child_pid: Option<u32>,
    pub child_pgid: Option<i32>,
    pub child_kind: Option<String>,
    pub session_started_at: i64,
    pub session_start_offset: Offsets,
    pub build_started_at: Option<i64>,
    pub build_finished_at: Option<i64>,
    pub build_exit_code: Option<i32>,
    pub run_started_at: Option<i64>,
    pub run_finished_at: Option<i64>,
    pub run_exit_code: Option<i32>,
    pub ready_at: Option<i64>,
    pub ready_via: Option<String>,
    pub probe: ProbeState,
    pub config_hash: String,
    pub agproc_version: String,
    pub updated_at: i64,
}

impl State {
    pub fn new(service: &str, generation: u64, config_hash: &str) -> Self {
        Self {
            schema: SCHEMA,
            service: service.to_string(),
            generation,
            phase: Phase::Stopped,
            runner_pid: std::process::id(),
            runner_start_ticks: procinfo::pid_start_ticks(std::process::id()).unwrap_or(0),
            child_pid: None,
            child_pgid: None,
            child_kind: None,
            session_started_at: now_unix_ms(),
            session_start_offset: Offsets::default(),
            build_started_at: None,
            build_finished_at: None,
            build_exit_code: None,
            run_started_at: None,
            run_finished_at: None,
            run_exit_code: None,
            ready_at: None,
            ready_via: None,
            probe: ProbeState::default(),
            config_hash: config_hash.to_string(),
            agproc_version: env!("CARGO_PKG_VERSION").to_string(),
            updated_at: now_unix_ms(),
        }
    }

    /// Is the runner that wrote this record still the same live process?
    pub fn runner_live(&self) -> bool {
        if !procinfo::pid_alive(self.runner_pid) {
            return false;
        }
        match procinfo::pid_start_ticks(self.runner_pid) {
            Some(ticks) if self.runner_start_ticks != 0 => ticks == self.runner_start_ticks,
            // Without a recorded start time, fall back to liveness only.
            _ => true,
        }
    }

    /// Recorded phase, downgraded to `Stale` when the runner vanished before
    /// recording an outcome.
    pub fn effective_phase(&self) -> Phase {
        if self.runner_live() {
            self.phase
        } else if matches!(
            self.phase,
            Phase::Building | Phase::Starting | Phase::Running
        ) {
            Phase::Stale
        } else {
            self.phase
        }
    }

    pub fn uptime_ms(&self) -> i64 {
        now_unix_ms() - self.session_started_at
    }
}

/// Result of reading a state file: either a record, or a description of why
/// the file could not be used.
#[derive(Debug, Default)]
pub struct Loaded {
    pub state: Option<State>,
    pub problem: Option<String>,
}

pub fn load(path: &Path) -> Loaded {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Loaded::default(),
        Err(err) => {
            return Loaded {
                state: None,
                problem: Some(format!("cannot read {}: {err}", path.display())),
            };
        }
    };
    match serde_json::from_slice::<State>(&raw) {
        Ok(state) if state.schema == SCHEMA => Loaded {
            state: Some(state),
            problem: None,
        },
        Ok(state) => Loaded {
            state: None,
            problem: Some(format!(
                "{} was written with state schema {} (this agproc uses {SCHEMA}); ignoring it",
                path.display(),
                state.schema
            )),
        },
        Err(err) => Loaded {
            state: None,
            problem: Some(format!("cannot parse {}: {err}", path.display())),
        },
    }
}

pub fn write(project: &Project, state: &mut State) -> Result<()> {
    state.updated_at = now_unix_ms();
    let path = project.state_path(&state.service);
    let body = serde_json::to_vec_pretty(state).context("cannot serialize state")?;
    write_atomic(&path, &project.tmp_dir(), &body)
        .with_context(|| format!("cannot write {}", path.display()))
}

/// `agproc stop` drops this marker before signalling, so the runner can tell a
/// user-requested stop from a crash.
pub fn mark_stop_requested(project: &Project, service: &str) -> Result<()> {
    let path = project.stop_marker(service);
    std::fs::write(&path, now_unix_ms().to_string())
        .with_context(|| format!("cannot write {}", path.display()))
}

pub fn stop_requested(project: &Project, service: &str) -> bool {
    project.stop_marker(service).exists()
}

pub fn clear_stop_requested(project: &Project, service: &str) {
    let _ = std::fs::remove_file(project.stop_marker(service));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(dir: &Path) -> Project {
        Project {
            root: dir.to_path_buf(),
            config_path: dir.join("agproc.toml"),
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let project = project(dir.path());
        project.ensure_layout().unwrap();

        let mut state = State::new("backend", 3, "fnv1a64:abcd");
        state.phase = Phase::Running;
        state.ready_at = Some(now_unix_ms());
        state.probe = ProbeState {
            kind: "http-get".into(),
            target: "http://127.0.0.1:3100/healthz".into(),
            attempts: 2,
            last_error: None,
            foreign_port_pid: None,
        };
        write(&project, &mut state).unwrap();

        let loaded = load(&project.state_path("backend"));
        assert!(loaded.problem.is_none());
        let read = loaded.state.unwrap();
        assert_eq!(read.phase, Phase::Running);
        assert_eq!(read.generation, 3);
        // We are the runner we just recorded, so the record is live.
        assert!(read.runner_live());
        assert_eq!(read.effective_phase(), Phase::Running);
        assert_eq!(read.probe.attempts, 2);
    }

    #[test]
    fn dead_runner_is_stale_only_while_in_flight() {
        let mut state = State::new("svc", 1, "h");
        state.runner_pid = 0x7fff_fffe; // certainly not running
        state.phase = Phase::Running;
        assert!(!state.runner_live());
        assert_eq!(state.effective_phase(), Phase::Stale);

        // A terminal phase recorded before the runner exited stays as-is.
        state.phase = Phase::Exited;
        assert_eq!(state.effective_phase(), Phase::Exited);
    }

    #[test]
    fn pid_reuse_is_detected_by_starttime() {
        let mut state = State::new("svc", 1, "h");
        state.runner_pid = std::process::id();
        state.runner_start_ticks = 1; // wrong fingerprint for our own pid
        assert!(!state.runner_live());
    }

    #[test]
    fn unparsable_state_is_reported_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let project = project(dir.path());
        let path = project.state_path("svc");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();
        let loaded = load(&path);
        assert!(loaded.state.is_none());
        assert!(loaded.problem.is_some());

        let loaded = load(&project.state_path("absent"));
        assert!(loaded.state.is_none());
        assert!(loaded.problem.is_none());
    }

    #[test]
    fn phase_exit_codes_match_the_contract() {
        assert_eq!(Phase::Running.start_exit_code(), 0);
        assert_eq!(Phase::BuildFailed.start_exit_code(), 4);
        assert_eq!(Phase::RunFailed.start_exit_code(), 5);
        assert_eq!(Phase::Exited.start_exit_code(), 5);
        assert_eq!(Phase::ProbeFailed.start_exit_code(), 6);
        assert_eq!(Phase::Stopped.start_exit_code(), 8);
    }
}
