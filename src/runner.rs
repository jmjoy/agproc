//! `agproc __runner` — the detached per-service supervisor.
//!
//! One runner owns exactly one service for one session. It performs
//! build-cmd -> run-cmd -> probe, streams both pty streams into the
//! service log files, and is the single authority on the service's lifecycle:
//! the child exit status is recorded even if a probe result arrived a moment
//! earlier, so a probe can never report a service that is already dead.
//!
//! The runner never holds the orchestration lock and never talks to the CLI
//! directly; everything travels through the log files and the state file.

use anyhow::{Context, Result, anyhow};
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use std::collections::BTreeMap;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

use crate::config::{self, ProbeTarget, Service, Settings};
use crate::logstore::LogSink;
use crate::paths::Project;
use crate::port::{self, Ownership};
use crate::proc::{self, ChildWatch, Pty, ReadOutcome, SpawnSpec};
use crate::state::{self, Offsets, Phase, State};
use crate::util::{format_duration_ms, iso8601_local, now_unix_ms};
use crate::{exit, procinfo};

pub struct Args {
    pub config: PathBuf,
    pub service: String,
    pub generation: u64,
}

static TERMINATE: AtomicBool = AtomicBool::new(false);

extern "C" fn on_terminate(_signal: nix::libc::c_int) {
    // Async-signal-safe: only an atomic store.
    TERMINATE.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() -> Result<()> {
    let action = SigAction::new(
        SigHandler::Handler(on_terminate),
        SaFlags::empty(),
        SigSet::empty(),
    );
    // SAFETY: the handler only performs an atomic store.
    unsafe {
        for signal in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP] {
            sigaction(signal, &action)
                .with_context(|| format!("cannot install handler for {signal:?}"))?;
        }
    }
    Ok(())
}

/// What a supervised phase ended with.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Exited(i32),
    Ready { attempts: u32 },
    ProbeFailed {
        attempts: u32,
        failures: u32,
        last_error: String,
    },
    Terminated,
    TimedOut,
}

struct Session {
    project: Project,
    settings: Settings,
    service: Service,
    sink: LogSink,
    state: State,
}

impl Session {
    fn publish(&mut self) -> Result<()> {
        state::write(&self.project, &mut self.state)
    }

    fn marker(&mut self, text: &str) -> Result<()> {
        self.sink.marker(text)
    }

    fn stop_timeout(&self) -> Duration {
        Duration::from_secs(self.service.stop_timeout(&self.settings).max(1))
    }

    fn stop_requested(&self) -> bool {
        state::stop_requested(&self.project, &self.service.name)
    }

    /// Record a terminal phase and return the process exit code for it.
    fn finish(&mut self, phase: Phase, marker: &str) -> Result<i32> {
        self.marker(marker)?;
        self.state.phase = phase;
        self.publish()?;
        Ok(phase.start_exit_code())
    }
}

pub fn run(args: Args) -> i32 {
    match inner(args) {
        Ok(code) => code,
        Err(err) => {
            // The CLI tails the service's stderr log, where our stderr lands.
            eprintln!("===== RUNNER ERROR =====");
            eprintln!("{err:#}");
            exit::GENERIC
        }
    }
}

fn inner(args: Args) -> Result<i32> {
    let config_path = crate::paths::absolute(&args.config)?;
    let root = config_path
        .parent()
        .ok_or_else(|| anyhow!("config path has no parent: {}", config_path.display()))?
        .to_path_buf();
    let project = Project {
        root,
        config_path: config_path.clone(),
    };
    let loaded = config::load(&config_path)?;
    let service = loaded
        .config
        .service(&args.service)
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "service \"{}\" not found in {}",
                args.service,
                config_path.display()
            )
        })?;
    let settings = loaded.config.settings.clone();
    project.ensure_layout()?;
    install_signal_handlers()?;

    let sink = LogSink::open(&project, &service.name, settings.log_max_bytes)?;
    let target = service.target();
    let mut state = State::new(&service.name, args.generation, &loaded.hash);
    let offsets = sink.offsets();
    state.session_start_offset = Offsets {
        stdout: offsets.0,
        stderr: offsets.1,
    };
    state.probe.kind = target.kind_str().to_string();
    state.probe.target = target.describe();
    state.runner_pid = std::process::id();
    state.runner_start_ticks = procinfo::pid_start_ticks(std::process::id()).unwrap_or(0);
    state.child_kind = Some(if service.build_cmd.is_some() {
        "build".to_string()
    } else {
        "run".to_string()
    });

    let mut session = Session {
        project,
        settings,
        service,
        sink,
        state,
    };
    session.marker(&format!(
        "SESSION {}",
        iso8601_local(session.state.session_started_at)
    ))?;
    session.state.phase = if session.service.build_cmd.is_some() {
        Phase::Building
    } else {
        Phase::Starting
    };
    session.publish()?;

    let cwd = session.service.cwd_path(&session.project);
    let shell = session.settings.shell.clone();
    let env = session.service.env.clone();
    let stop_timeout = session.stop_timeout();

    // ---------------------------------------------------------------- build
    if let Some(build_cmd) = session.service.build_cmd.clone() {
        session.marker("BUILDING")?;
        session.state.build_started_at = Some(now_unix_ms());
        session.publish()?;

        let argv = build_cmd.argv(&shell);
        let timeout = (session.service.build_timeout() > 0)
            .then(|| Duration::from_secs(session.service.build_timeout()));
        let mut running = match Running::start(&argv, &cwd, &env) {
            Ok(running) => running,
            Err(err) => {
                session.state.build_finished_at = Some(now_unix_ms());
                return session.finish(
                    Phase::BuildFailed,
                    &format!("BUILD FAILED (cannot spawn: {err:#})"),
                );
            }
        };
        session.state.child_pid = Some(running.pid);
        session.state.child_pgid = Some(running.pgid);
        session.publish()?;

        let deadline = timeout.map(|t| Instant::now() + t);
        let outcome = running.supervise(&mut session.sink, deadline, None, &TERMINATE)?;
        running.terminate(stop_timeout)?;

        session.state.build_finished_at = Some(now_unix_ms());
        match outcome {
            Outcome::Exited(0) => {
                session.state.build_exit_code = Some(0);
                session.marker("BUILD SUCCEED")?;
                session.publish()?;
            }
            Outcome::Exited(code) => {
                session.state.build_exit_code = Some(code);
                return session.finish(Phase::BuildFailed, &format!("BUILD FAILED (exit code {code})"));
            }
            Outcome::TimedOut => {
                let seconds = session.service.build_timeout();
                return session.finish(
                    Phase::BuildFailed,
                    &format!("BUILD FAILED (timeout after {seconds}s)"),
                );
            }
            Outcome::Terminated => {
                let marker = if session.stop_requested() {
                    "STOPPED (build cancelled)"
                } else {
                    "STOPPED (signal received while building)"
                };
                return session.finish(Phase::Stopped, marker);
            }
            Outcome::Ready { .. } | Outcome::ProbeFailed { .. } => {
                unreachable!("the build phase has no probe")
            }
        }
    }

    // ------------------------------------------------------------------ run
    let preflight = port::preflight(&target, &session.settings);
    if let Some(foreign) = &preflight {
        session.state.probe.foreign_port_pid = Some(foreign.pid);
        let port = target.local_port().unwrap_or(0);
        session.marker(&format!(
            "WARNING: PORT {port} ALREADY IN USE BY {}",
            foreign.describe()
        ))?;
    }

    session.state.phase = Phase::Starting;
    session.state.run_started_at = Some(now_unix_ms());
    session.state.child_kind = Some("run".to_string());
    session.marker("RUNNING")?;
    session.publish()?;

    let argv = session.service.run_cmd.argv(&shell);
    let mut running = match Running::start(&argv, &cwd, &env) {
        Ok(running) => running,
        Err(err) => {
            session.state.run_finished_at = Some(now_unix_ms());
            return session.finish(
                Phase::RunFailed,
                &format!("RUNNING FAILED (cannot spawn: {err:#})"),
            );
        }
    };
    session.state.child_pid = Some(running.pid);
    session.state.child_pgid = Some(running.pgid);
    session.publish()?;

    let mut probe = ProbeRunner::new(&session.service, &target, Instant::now());
    let outcome = running.supervise(&mut session.sink, None, Some(&mut probe), &TERMINATE)?;

    match outcome {
        Outcome::Ready { attempts } => {
            match port::verify(
                &target,
                &session.settings,
                running.pgid,
                preflight.as_ref(),
            ) {
                Ownership::ConfirmedForeign(listener) => {
                    let port = target.local_port().unwrap_or(0);
                    let reason = format!(
                        "port {port} is owned by {}, not by service \"{}\"",
                        listener.describe(),
                        session.service.name
                    );
                    session.state.probe.last_error = Some(reason.clone());
                    session.state.probe.attempts = attempts;
                    running.terminate(stop_timeout)?;
                    return session.finish(
                        Phase::ProbeFailed,
                        &format!("PROBE FAILED: {reason}"),
                    );
                }
                Ownership::Unexpected(listener) => {
                    let port = target.local_port().unwrap_or(0);
                    session.marker(&format!(
                        "WARNING: PORT {port} IS HELD BY {} (not part of this service); the probe result may be misleading",
                        listener.describe()
                    ))?;
                }
                Ownership::Ours | Ownership::Unknown => {}
            }

            session.state.ready_at = Some(now_unix_ms());
            session.state.ready_via = Some(target.kind_str().to_string());
            session.state.probe.attempts = attempts;
            session.state.phase = Phase::Running;
            let marker = match target {
                ProbeTarget::None => "PROBE PASSED (NO PROBE CONFIGURED)".to_string(),
                _ => format!("PROBE PASSED (attempt {attempts})"),
            };
            session.marker(&marker)?;
            session.publish()?;
        }
        Outcome::ProbeFailed {
            attempts,
            failures,
            last_error,
        } => {
            session.state.probe.attempts = attempts;
            session.state.probe.last_error = Some(last_error.clone());
            // Per design: stop the child, so the next `start` rebuilds and runs
            // cleanly instead of tripping over a half-ready process.
            running.terminate(stop_timeout)?;
            return session.finish(
                Phase::ProbeFailed,
                &format!(
                    "PROBE FAILED: {failures} consecutive failures, last: {last_error} ({})",
                    target.describe()
                ),
            );
        }
        Outcome::Exited(code) => {
            running.terminate(stop_timeout)?;
            session.state.run_exit_code = Some(code);
            session.state.run_finished_at = Some(now_unix_ms());
            if session.stop_requested() {
                return session.finish(Phase::Stopped, "STOPPED");
            }
            return session.finish(Phase::RunFailed, &format!("RUNNING FAILED (exit code {code})"));
        }
        Outcome::Terminated => {
            running.terminate(stop_timeout)?;
            let marker = if session.stop_requested() {
                "STOPPED"
            } else {
                "STOPPED (signal received)"
            };
            return session.finish(Phase::Stopped, marker);
        }
        Outcome::TimedOut => unreachable!("the run phase has no timeout"),
    }

    // ------------------------------------------------------- wait after ready
    let started = Instant::now();
    let outcome = running.supervise(&mut session.sink, None, None, &TERMINATE)?;
    running.terminate(stop_timeout)?;
    session.state.run_finished_at = Some(now_unix_ms());
    let uptime = format_duration_ms(started.elapsed().as_millis() as i64);

    match outcome {
        Outcome::Exited(code) => {
            session.state.run_exit_code = Some(code);
            if session.stop_requested() {
                return session.finish(Phase::Stopped, "STOPPED");
            }
            session.finish(Phase::Exited, &format!("SERVICE EXITED (exit code {code}, ready for {uptime})"))
        }
        Outcome::Terminated => {
            if session.stop_requested() {
                return session.finish(Phase::Stopped, "STOPPED");
            }
            session.finish(Phase::Exited, &format!("SERVICE EXITED (terminated, ready for {uptime})"))
        }
        other => {
            running.terminate(stop_timeout)?;
            session.finish(
                Phase::Exited,
                &format!("SERVICE EXITED ({other:?}, ready for {uptime})"),
            )
        }
    }
}

/// A child that is running (or has just finished) inside its own process group.
struct Running {
    pid: u32,
    pgid: i32,
    out_pty: Pty,
    err_pty: Pty,
    watch: ChildWatch,
    exited: Option<i32>,
}

impl Running {
    fn start(argv: &[String], cwd: &Path, env: &BTreeMap<String, String>) -> Result<Self> {
        let out_pty = proc::open_pty()?;
        let err_pty = proc::open_pty()?;
        let child = proc::spawn(&SpawnSpec {
            argv,
            cwd,
            env,
            stdout: out_pty.slave.as_raw_fd(),
            stderr: err_pty.slave.as_raw_fd(),
        })?;
        let pid = child.id();
        Ok(Self {
            pid,
            pgid: pid as i32,
            out_pty,
            err_pty,
            watch: ChildWatch::spawn(child),
            exited: None,
        })
    }

    fn supervise(
        &mut self,
        sink: &mut LogSink,
        deadline: Option<Instant>,
        mut probe: Option<&mut ProbeRunner>,
        terminate: &AtomicBool,
    ) -> Result<Outcome> {
        if let Some(code) = self.exited {
            return Ok(Outcome::Exited(code));
        }
        loop {
            if terminate.load(Ordering::SeqCst) {
                return Ok(Outcome::Terminated);
            }
            let ready = proc::poll_readable(
                &[
                    self.out_pty.master.as_raw_fd(),
                    self.err_pty.master.as_raw_fd(),
                ],
                100,
            )?;
            if ready[0] {
                self.pump(sink, true)?;
            }
            if ready[1] {
                self.pump(sink, false)?;
            }

            if let Some(status) = self.watch.try_exit() {
                let code = proc::exit_code_of(&status);
                self.exited = Some(code);
                // Let the child's last words reach the log before the outcome
                // marker is written.
                self.drain(sink, Duration::from_millis(200));
                return Ok(Outcome::Exited(code));
            }
            if let Some(deadline) = deadline
                && Instant::now() >= deadline
            {
                return Ok(Outcome::TimedOut);
            }
            if let Some(probe) = probe.as_deref_mut()
                && let Some(step) = probe.step(sink)?
            {
                return Ok(match step {
                    ProbeStep::Ready { attempts } => Outcome::Ready { attempts },
                    ProbeStep::Failed {
                        attempts,
                        failures,
                        last_error,
                    } => Outcome::ProbeFailed {
                        attempts,
                        failures,
                        last_error,
                    },
                });
            }
        }
    }

    fn pump(&mut self, sink: &mut LogSink, is_stdout: bool) -> Result<()> {
        let master = if is_stdout {
            &self.out_pty.master
        } else {
            &self.err_pty.master
        };
        loop {
            match proc::read_available(master) {
                ReadOutcome::Data(bytes) => {
                    if is_stdout {
                        sink.write_stdout(&bytes)?;
                    } else {
                        sink.write_stderr(&bytes)?;
                    }
                }
                ReadOutcome::WouldBlock | ReadOutcome::Eof => return Ok(()),
            }
        }
    }

    /// Keep reading until both ptys have been quiet for `quiet`.
    fn drain(&mut self, sink: &mut LogSink, quiet: Duration) {
        let mut last = Instant::now();
        while last.elapsed() < quiet {
            for is_stdout in [true, false] {
                let master = if is_stdout {
                    &self.out_pty.master
                } else {
                    &self.err_pty.master
                };
                match proc::read_available(master) {
                    ReadOutcome::Data(bytes) => {
                        let written = if is_stdout {
                            sink.write_stdout(&bytes)
                        } else {
                            sink.write_stderr(&bytes)
                        };
                        if written.is_ok() {
                            last = Instant::now();
                        }
                    }
                    ReadOutcome::WouldBlock | ReadOutcome::Eof => {}
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// SIGTERM the whole group, escalate to SIGKILL after `grace`.
    fn terminate(&mut self, grace: Duration) -> Result<()> {
        proc::signal_group(self.pgid, Signal::SIGTERM)?;
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline && proc::group_exists(self.pgid) {
            std::thread::sleep(Duration::from_millis(50));
        }
        if proc::group_exists(self.pgid) {
            proc::signal_group(self.pgid, Signal::SIGKILL)?;
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline && proc::group_exists(self.pgid) {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(())
    }
}

enum ProbeStep {
    Ready {
        attempts: u32,
    },
    Failed {
        attempts: u32,
        failures: u32,
        last_error: String,
    },
}

/// Drives probe attempts without blocking log forwarding: each attempt runs on
/// its own thread and is collected on the next loop iteration.
struct ProbeRunner {
    target: ProbeTarget,
    timeout: Duration,
    period: Duration,
    threshold: u32,
    next_at: Instant,
    pending: bool,
    tx: Sender<Result<(), String>>,
    rx: Receiver<Result<(), String>>,
    attempts: u32,
    failures: u32,
}

impl ProbeRunner {
    fn new(service: &Service, target: &ProbeTarget, now: Instant) -> Self {
        let probe = service.probe.as_ref();
        let (tx, rx) = channel();
        Self {
            target: target.clone(),
            timeout: Duration::from_secs(probe.map(|p| p.timeout_seconds).unwrap_or(2).max(1)),
            period: Duration::from_secs(probe.map(|p| p.period_seconds).unwrap_or(1).max(1)),
            threshold: probe.map(|p| p.failure_threshold).unwrap_or(1).max(1),
            next_at: now + Duration::from_secs(probe.map(|p| p.initial_delay_seconds).unwrap_or(1)),
            pending: false,
            tx,
            rx,
            attempts: 0,
            failures: 0,
        }
    }

    fn step(&mut self, sink: &mut LogSink) -> Result<Option<ProbeStep>> {
        if self.pending {
            match self.rx.try_recv() {
                Ok(Ok(())) => {
                    self.pending = false;
                    self.attempts += 1;
                    return Ok(Some(ProbeStep::Ready {
                        attempts: self.attempts,
                    }));
                }
                Ok(Err(reason)) => {
                    self.pending = false;
                    self.attempts += 1;
                    self.failures += 1;
                    sink.marker(&format!(
                        "PROBE ATTEMPT {}/{} FAILED: {reason} ({})",
                        self.attempts,
                        self.threshold,
                        self.target.describe()
                    ))?;
                    if self.failures >= self.threshold {
                        return Ok(Some(ProbeStep::Failed {
                            attempts: self.attempts,
                            failures: self.failures,
                            last_error: reason,
                        }));
                    }
                    self.next_at = Instant::now() + self.period;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.pending = false,
            }
            return Ok(None);
        }

        if Instant::now() >= self.next_at {
            let target = self.target.clone();
            let timeout = self.timeout;
            let tx = self.tx.clone();
            let spawned = std::thread::Builder::new()
                .name("agproc-probe".to_string())
                .spawn(move || {
                    let _ = tx.send(crate::probe::attempt(&target, timeout));
                });
            match spawned {
                Ok(_) => self.pending = true,
                Err(err) => {
                    return Err(anyhow!("cannot spawn probe thread: {err}"));
                }
            }
        }
        Ok(None)
    }
}
