//! `agproc stop` — stop the build or run process of a service.
//!
//! `stop` never takes the orchestration lock: it has to work while a `start`
//! is still in flight. It drops a marker so the runner can distinguish a
//! requested stop from a crash, signals the service's process group and the
//! runner, escalates to SIGKILL when needed, and finally records the outcome
//! itself if the runner did not live long enough to do so.
//!
//! Stopping several services runs them in parallel, exactly like `start`: a
//! graceful drain of one service must not add to the shutdown time of the next.

use anyhow::Result;
use nix::sys::signal::Signal;
use std::time::{Duration, Instant};

use crate::cli::Failure;
use crate::cmd::Prefix;
use crate::config::{Config, Service, Settings};
use crate::exit;
use crate::paths::Project;
use crate::proc;
use crate::procinfo;
use crate::state::{self, Phase, State};
use crate::util::now_unix_ms;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    Stopped,
    Forced,
    NotRunning,
    CleanedUp,
}

/// Stop every selected service concurrently.
pub fn run(project: &Project, config: &Config, services: Vec<&Service>) -> Result<i32, Failure> {
    let settings = config.settings.clone();
    let multi = services.len() > 1;
    let mut results: Vec<(String, Result<StopOutcome, String>)> = Vec::new();

    if services.len() == 1 {
        let service = services[0];
        let prefix = Prefix::service(&service.name, multi);
        let outcome = stop_service(project, &settings, service, &prefix)
            .map_err(|err| format!("{err:#}"));
        results.push((service.name.clone(), outcome));
    } else {
        let mut handles = Vec::new();
        for service in services {
            let project = project.clone();
            let settings = settings.clone();
            let name = service.name.clone();
            let service = service.clone();
            handles.push(std::thread::spawn(move || {
                let prefix = Prefix::service(&name, true);
                let outcome = stop_service(&project, &settings, &service, &prefix)
                    .map_err(|err| format!("{err:#}"));
                (name, outcome)
            }));
        }
        for handle in handles {
            match handle.join() {
                Ok(result) => results.push(result),
                Err(_) => results.push((
                    "<thread>".to_string(),
                    Err("stop worker panicked".to_string()),
                )),
            }
        }
    }

    let mut failed = false;
    for (name, result) in &results {
        if let Err(reason) = result {
            Prefix::plain().marker(&format!("STOP FAILED: {name} ({reason})"));
            failed = true;
        }
    }
    Ok(if failed { exit::GENERIC } else { exit::OK })
}

pub fn stop_service(
    project: &Project,
    settings: &Settings,
    service: &Service,
    prefix: &Prefix,
) -> Result<StopOutcome> {
    let name = service.name.as_str();
    let state_path = project.state_path(name);
    let loaded = state::load(&state_path);
    if let Some(problem) = &loaded.problem {
        prefix.diagnostic(problem);
    }
    let Some(state) = loaded.state else {
        state::clear_stop_requested(project, name);
        prefix.marker("NOT RUNNING");
        return Ok(StopOutcome::NotRunning);
    };

    let grace = Duration::from_secs(service.stop_timeout(settings).max(1));
    let phase = state.effective_phase();

    if !matches!(phase, Phase::Building | Phase::Starting | Phase::Running) {
        // Already finished: only clean up a process group orphaned by a killed
        // runner.
        state::clear_stop_requested(project, name);
        if reap_orphan_group(&state, grace)? {
            prefix.marker(&format!(
                "STOPPED (killed leftover process group of the {})",
                phase.display()
            ));
            return Ok(StopOutcome::CleanedUp);
        }
        prefix.marker("NOT RUNNING");
        return Ok(StopOutcome::NotRunning);
    }

    state::mark_stop_requested(project, name)?;
    if let Some(pgid) = state.child_pgid {
        proc::signal_group(pgid, Signal::SIGTERM)?;
    }
    proc::signal_pid(state.runner_pid, Signal::SIGTERM)?;

    wait_dead(state.runner_pid, grace + Duration::from_secs(2));

    let mut forced = false;
    if procinfo::pid_alive(state.runner_pid) {
        proc::signal_pid(state.runner_pid, Signal::SIGKILL)?;
        forced = true;
        wait_dead(state.runner_pid, Duration::from_secs(2));
    }
    if let Some(pgid) = state.child_pgid
        && proc::group_exists(pgid)
    {
        proc::signal_group(pgid, Signal::SIGKILL)?;
        forced = true;
    }

    // If the runner was killed before it could record the outcome, do it here.
    let after = state::load(&state_path);
    let recorded = after
        .state
        .as_ref()
        .map(|s| {
            s.generation == state.generation
                && matches!(
                    s.effective_phase(),
                    Phase::Stopped | Phase::Exited | Phase::RunFailed | Phase::BuildFailed
                )
        })
        .unwrap_or(false);
    if !recorded {
        let mut updated = state.clone();
        updated.phase = Phase::Stopped;
        updated.run_finished_at = Some(now_unix_ms());
        state::write(project, &mut updated)?;
    }
    state::clear_stop_requested(project, name);

    prefix.marker(if forced { "STOPPED (forced)" } else { "STOPPED" });
    Ok(if forced {
        StopOutcome::Forced
    } else {
        StopOutcome::Stopped
    })
}

fn wait_dead(pid: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline && procinfo::pid_alive(pid) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Kill a process group left behind by a runner that is already gone (a
/// `kill -9` on the runner, for instance). Returns whether anything was there.
pub fn reap_orphan_group(state: &State, grace: Duration) -> Result<bool> {
    let Some(pgid) = state.child_pgid else {
        return Ok(false);
    };
    if !proc::group_exists(pgid) {
        return Ok(false);
    }
    proc::signal_group(pgid, Signal::SIGTERM)?;
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline && proc::group_exists(pgid) {
        std::thread::sleep(Duration::from_millis(50));
    }
    if proc::group_exists(pgid) {
        proc::signal_group(pgid, Signal::SIGKILL)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && proc::group_exists(pgid) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    Ok(true)
}
