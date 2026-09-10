//! `agproc start` / `agproc restart` — build, run and wait for the probe.
//!
//! The CLI is deliberately thin: it takes the orchestration lock, spawns a
//! detached runner for the service, forwards the runner's log output, and waits
//! until the state file reaches a settled phase. All the process handling lives
//! in the runner, so a killed CLI never leaves a half-finished start behind.

use anyhow::Result;
use std::time::{Duration, Instant};

use crate::cli::Failure;
use crate::cmd::stop::{reap_orphan_group, stop_service};
use crate::cmd::{Prefix, select_services};
use crate::config::{Config, Service, Settings};
use crate::exit;
use crate::lock::Lock;
use crate::logstore::LogRelay;
use crate::paths::Project;
use crate::proc;
use crate::procinfo;
use crate::state::{self, Phase, State};
use crate::util::format_duration_ms;

pub struct Request {
    pub services: Vec<String>,
    pub restart: bool,
    pub timeout: Option<Duration>,
}

/// A failed start, reduced to the information the aggregate summary needs.
#[derive(Debug)]
struct Failed {
    code: i32,
    reason: String,
}

impl Failed {
    fn new(code: i32, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }
}

impl From<anyhow::Error> for Failed {
    fn from(err: anyhow::Error) -> Self {
        Failed::new(exit::GENERIC, format!("{err:#}"))
    }
}

#[derive(Debug)]
enum Started {
    Ready,
    AlreadyRunning,
}

pub fn run(project: &Project, config: &Config, request: Request) -> Result<i32, Failure> {
    if let Some(warning) = project.layout_warning() {
        Prefix::plain().marker(&format!("WARNING: {warning}"));
    }
    project.ensure_layout().map_err(Failure::from)?;

    let services: Vec<Service> = select_services(config, &request.services)
        .map_err(Failure::config)?
        .into_iter()
        .cloned()
        .collect();
    let settings = config.settings.clone();
    let hash = config_hash(project);
    let multi = services.len() > 1;

    let mut results: Vec<(String, Result<Started, Failed>)> = Vec::new();
    if services.len() == 1 {
        let service = &services[0];
        let prefix = Prefix::service(&service.name, multi);
        results.push((
            service.name.clone(),
            start_one(
                project,
                &settings,
                service,
                &hash,
                request.restart,
                request.timeout,
                &prefix,
            ),
        ));
    } else {
        let mut handles = Vec::new();
        for service in services {
            let project = project.clone();
            let settings = settings.clone();
            let hash = hash.clone();
            let (restart, timeout) = (request.restart, request.timeout);
            handles.push(std::thread::spawn(move || {
                let prefix = Prefix::service(&service.name, true);
                let name = service.name.clone();
                let outcome = start_one(
                    &project,
                    &settings,
                    &service,
                    &hash,
                    restart,
                    timeout,
                    &prefix,
                );
                (name, outcome)
            }));
        }
        for handle in handles {
            match handle.join() {
                Ok(result) => results.push(result),
                Err(_) => results.push((
                    "<thread>".to_string(),
                    Err(Failed::new(exit::GENERIC, "start worker panicked")),
                )),
            }
        }
    }

    let mut first_failure: Option<Failed> = None;
    for (name, result) in &results {
        if let Err(failed) = result {
            Prefix::plain().marker(&format!("START FAILED: {name} ({})", failed.reason));
            if first_failure.is_none() {
                first_failure = Some(Failed::new(failed.code, failed.reason.clone()));
            }
        }
    }
    Ok(match first_failure {
        Some(failed) => failed.code,
        None => exit::OK,
    })
}

fn config_hash(project: &Project) -> String {
    crate::config::load(&project.config_path)
        .map(|loaded| loaded.hash)
        .unwrap_or_default()
}

fn start_one(
    project: &Project,
    settings: &Settings,
    service: &Service,
    config_hash: &str,
    restart: bool,
    timeout: Option<Duration>,
    prefix: &Prefix,
) -> Result<Started, Failed> {
    let name = service.name.as_str();
    let state_path = project.state_path(name);
    let lock_path = project.lock_path(name);

    // One start/restart per service at a time. The lock covers build, run and
    // probe; a second invocation reports who is holding it instead of racing.
    let lock = Lock::try_acquire(&lock_path).map_err(Failed::from)?;
    let Some(_lock) = lock else {
        let holder = Lock::holder_pid(&lock_path).unwrap_or(0);
        let phase = state::load(&state_path)
            .state
            .map(|s| s.effective_phase().display())
            .unwrap_or("unknown");
        prefix.marker(&format!("START IN PROGRESS (pid {holder}, phase {phase})"));
        return Err(Failed::new(
            exit::LOCKED,
            "another start/restart is in progress",
        ));
    };

    let loaded = state::load(&state_path);
    if let Some(problem) = &loaded.problem {
        prefix.diagnostic(problem);
    }
    let previous = loaded.state;
    // A runner killed with SIGKILL leaves its service process group behind.
    // Adopt and reap it, otherwise the next run-cmd cannot bind its port.
    if let Some(state) = &previous
        && !state.runner_live()
        && let Some(pgid) = state.child_pgid
        && proc::group_exists(pgid)
    {
        prefix.marker(&format!(
            "CLEANING UP LEFTOVER PROCESS GROUP (pgid {pgid})"
        ));
        reap_orphan_group(state, Duration::from_secs(settings.stop_timeout_seconds.max(1)))
            .map_err(Failed::from)?;
    }
    let current = previous.clone().filter(|s| s.runner_live());

    if let Some(state) = &current {
        let phase = state.effective_phase();
        let in_flight = matches!(phase, Phase::Building | Phase::Starting | Phase::Running);
        if restart {
            if in_flight {
                let pid = state.child_pid.unwrap_or(state.runner_pid);
                prefix.marker(&format!("RESTARTING (stopping pid {pid})"));
                stop_service(project, settings, service, prefix).map_err(Failed::from)?;
            }
        } else {
            match phase {
                Phase::Running => {
                    let pid = state.child_pid.unwrap_or(state.runner_pid);
                    prefix.marker(&format!(
                        "ALREADY RUNNING (pid {pid}, uptime {}, ready)",
                        format_duration_ms(state.uptime_ms())
                    ));
                    if !config_hash.is_empty() && state.config_hash != config_hash {
                        prefix.marker(&format!(
                            "WARNING: CONFIG CHANGED SINCE START (run `agproc restart {name}` to apply it)"
                        ));
                    }
                    return Ok(Started::AlreadyRunning);
                }
                Phase::Building | Phase::Starting => {
                    prefix.marker(&format!(
                        "START IN PROGRESS (pid {}, phase {})",
                        state.runner_pid,
                        phase.display()
                    ));
                    return Err(Failed::new(
                        exit::LOCKED,
                        format!("service is already {}", phase.display()),
                    ));
                }
                _ => {}
            }
        }
    }

    let generation = current.as_ref().map(|s| s.generation).unwrap_or(0) + 1;

    let exe = std::env::current_exe().map_err(|err| {
        Failed::new(
            exit::GENERIC,
            format!("cannot locate the agproc binary: {err}"),
        )
    })?;
    let args = vec![
        "__runner".to_string(),
        "--config".to_string(),
        project.config_path.display().to_string(),
        "--service".to_string(),
        name.to_string(),
        "--generation".to_string(),
        generation.to_string(),
    ];
    // The runner's own stderr joins the console stream, so a failure to start
    // is visible to whoever is watching.
    let runner_pid = proc::spawn_detached(&exe, &args, Some(&project.console_stderr(name)))
        .map_err(Failed::from)?;

    // Follow the console stream: markers, build output and (until readiness)
    // run output. The runner truncates it before publishing its state, so we
    // wait for our generation and only then start reading from the beginning.
    let mut relay = LogRelay::new(
        project.console_stdout(name),
        project.console_stderr(name),
        prefix.out.clone(),
        prefix.err.clone(),
    );

    let started = Instant::now();
    // Give the runner a moment to publish its first state record (it truncates
    // the console stream before publishing, so reading from 0 cannot pick up a
    // previous session's bytes).
    let publish_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < publish_deadline {
        if let Some(state) = state::load(&state_path).state
            && state.generation == generation
            && state.runner_pid == runner_pid
        {
            break;
        }
        if !procinfo::pid_alive(runner_pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // Even when the runner never published (it failed to start), its stderr went
    // to the console stream: read it so the failure is visible.
    relay.seek(0, 0).map_err(Failed::from)?;

    loop {
        relay.pump().map_err(Failed::from)?;

        if let Some(state) = state::load(&state_path).state
            && state.generation == generation
            && state.effective_phase().settled()
        {
            drain_logs(&mut relay)?;
            return settle(&state);
        }

        if !procinfo::pid_alive(runner_pid) {
            drain_logs(&mut relay)?;
            if let Some(state) = state::load(&state_path).state
                && state.generation == generation
                && state.effective_phase().settled()
            {
                return settle(&state);
            }
            prefix.marker(&format!("RUNNER FAILED TO START (pid {runner_pid})"));
            return Err(Failed::new(
                exit::GENERIC,
                "runner exited before reaching a settled phase (see the service logs)",
            ));
        }

        if let Some(limit) = timeout
            && started.elapsed() > limit
        {
            let phase = state::load(&state_path)
                .state
                .map(|s| s.effective_phase().display())
                .unwrap_or("unknown");
            prefix.marker(&format!(
                "STILL STARTING (runner pid {runner_pid}, phase {phase})"
            ));
            relay.flush();
            return Err(Failed::new(
                exit::GENERIC,
                "timed out waiting for the probe (the runner keeps working; use `agproc ps`)",
            ));
        }

        std::thread::sleep(Duration::from_millis(25));
    }
}

fn drain_logs(relay: &mut LogRelay) -> Result<(), Failed> {
    // Let the last writes land so the outcome marker is never printed before
    // the output that explains it.
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(20));
        relay.pump().map_err(Failed::from)?;
    }
    relay.flush();
    Ok(())
}

fn settle(state: &State) -> Result<Started, Failed> {
    let phase = state.effective_phase();
    match phase {
        Phase::Running => Ok(Started::Ready),
        Phase::BuildFailed => Err(Failed::new(exit::BUILD_FAILED, "build failed")),
        Phase::RunFailed => Err(Failed::new(exit::RUN_FAILED, "running failed")),
        Phase::Exited => Err(Failed::new(
            exit::RUN_FAILED,
            "running failed (exited right after the probe passed)",
        )),
        Phase::ProbeFailed => {
            Err(Failed::new(exit::PROBE_FAILED, "probe failed"))
        }
        Phase::Stopped => Err(Failed::new(
            exit::SUPERSEDED,
            "stopped by another agproc invocation",
        )),
        Phase::Stale => Err(Failed::new(
            exit::RUN_FAILED,
            "the runner disappeared before recording an outcome",
        )),
        other => Err(Failed::new(
            exit::GENERIC,
            format!("unexpected phase \"{}\"", other.as_str()),
        )),
    }
}
