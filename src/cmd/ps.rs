//! `agproc ps` — what is actually running.
//!
//! Every row is derived from the state file plus a liveness check on the runner
//! (pid + `/proc/<pid>/stat` start time), so the answer stays correct long
//! after the CLI that started the service exited.

use anyhow::Result;
use serde::Serialize;

use crate::cli::Failure;
use crate::cmd::{Prefix, select_services};
use crate::config::Config;
use crate::exit;
use crate::paths::Project;
use crate::state::{self, Phase};
use crate::util::{format_duration_ms, iso8601_local};

pub struct Request {
    pub services: Vec<String>,
    pub json: bool,
}

#[derive(Serialize)]
struct ProbeInfo {
    kind: String,
    target: String,
    attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
}

#[derive(Serialize)]
struct Row {
    name: String,
    phase: String,
    phase_display: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    runner_pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    child_pid: Option<u32>,
    ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uptime_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    build_exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    probe: Option<ProbeInfo>,
    config_changed: bool,
    detail: String,
    log_stdout: String,
    log_stderr: String,
}

#[derive(Serialize)]
struct Report {
    version: String,
    root: String,
    services: Vec<Row>,
}

pub fn run(project: &Project, config: &Config, request: Request) -> Result<i32, Failure> {
    let services = select_services(config, &request.services).map_err(Failure::config)?;
    if let Some(warning) = project.layout_warning() {
        Prefix::plain().marker(&format!("WARNING: {warning}"));
    }

    let mut rows = Vec::new();
    let current_hash = crate::config::load(&project.config_path)
        .map(|loaded| loaded.hash)
        .unwrap_or_default();
    for service in services {
        let loaded = state::load(&project.state_path(&service.name));
        if let Some(problem) = &loaded.problem {
            Prefix::plain().diagnostic(problem);
        }
        let config_changed = loaded
            .state
            .as_ref()
            .map(|state| !state.config_hash.is_empty() && state.config_hash != current_hash)
            .unwrap_or(false);
        rows.push(row_for(service, loaded.state, config_changed, project));
    }

    if request.json {
        let report = Report {
            version: env!("CARGO_PKG_VERSION").to_string(),
            root: project.root.display().to_string(),
            services: rows,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
        return Ok(exit::OK);
    }

    print_table(&rows);
    Ok(exit::OK)
}

fn row_for(
    service: &crate::config::Service,
    state: Option<state::State>,
    config_changed: bool,
    project: &Project,
) -> Row {
    let target = service.target();
    let log_stdout = project.log_stdout(&service.name).display().to_string();
    let log_stderr = project.log_stderr(&service.name).display().to_string();

    let Some(state) = state else {
        return Row {
            name: service.name.clone(),
            phase: Phase::Stopped.as_str().to_string(),
            phase_display: Phase::Stopped.display().to_string(),
            pid: None,
            runner_pid: 0,
            child_pid: None,
            ready: false,
            started_at: None,
            uptime_seconds: None,
            build_exit_code: None,
            run_exit_code: None,
            probe: None,
            config_changed: false,
            detail: "never started".to_string(),
            log_stdout,
            log_stderr,
        };
    };

    let phase = state.effective_phase();
    let live = state.runner_live();
    let running = matches!(phase, Phase::Running | Phase::Starting | Phase::Building);
    let detail = match phase {
        Phase::Building => format!(
            "building for {}",
            format_duration_ms(state.uptime_ms())
        ),
        Phase::Starting => format!("probing ({})", target.describe()),
        Phase::Running => {
            let ready_ago = state
                .ready_at
                .map(|at| format_duration_ms(crate::util::now_unix_ms() - at))
                .unwrap_or_else(|| "?".to_string());
            format!("ready {ready_ago} ago, probe {}", state.probe.target)
        }
        Phase::BuildFailed => match state.build_exit_code {
            Some(code) => format!("build-cmd exited with {code}"),
            None => "build-cmd timed out".to_string(),
        },
        Phase::RunFailed => match state.run_exit_code {
            Some(code) => format!("run-cmd exited with {code} before the probe passed"),
            None => "run-cmd exited before the probe passed".to_string(),
        },
        Phase::Exited => match state.run_exit_code {
            Some(code) => format!("run-cmd exited with {code} after the probe passed"),
            None => "run-cmd terminated after the probe passed".to_string(),
        },
        Phase::ProbeFailed => match &state.probe.last_error {
            Some(err) => format!("{} {}: {err}", state.probe.kind, target.describe()),
            None => format!("{} {} never became ready", state.probe.kind, target.describe()),
        },
        Phase::ConfigFailed => format!(
            "configuration error (see .agproc/tmp/{}.console.stdout)",
            state.service
        ),
        Phase::Stopped => "stopped".to_string(),
        Phase::Stale => format!(
            "runner {} disappeared without recording an outcome",
            state.runner_pid
        ),
    };

    Row {
        name: state.service.clone(),
        phase: phase.as_str().to_string(),
        phase_display: phase.display().to_string(),
        pid: if running {
            state.child_pid.or(Some(state.runner_pid))
        } else {
            None
        },
        runner_pid: if live { state.runner_pid } else { 0 },
        child_pid: state.child_pid,
        ready: matches!(phase, Phase::Running | Phase::Exited),
        started_at: Some(iso8601_local(state.session_started_at)),
        uptime_seconds: if running {
            Some(state.uptime_ms() / 1000)
        } else {
            None
        },
        build_exit_code: state.build_exit_code,
        run_exit_code: state.run_exit_code,
        probe: Some(ProbeInfo {
            kind: state.probe.kind.clone(),
            target: state.probe.target.clone(),
            attempts: state.probe.attempts,
            last_error: state.probe.last_error.clone(),
        }),
        config_changed,
        detail,
        log_stdout,
        log_stderr,
    }
}

fn print_table(rows: &[Row]) {
    let name_width = rows
        .iter()
        .map(|row| row.name.len())
        .chain(std::iter::once(4))
        .max()
        .unwrap_or(4);
    let status_width = rows
        .iter()
        .map(|row| row.phase_display.len())
        .chain(std::iter::once(6))
        .max()
        .unwrap_or(6);

    println!(
        "{:<name_width$}  {:<status_width$}  {:<7}  {:<7}  DETAIL",
        "NAME", "STATUS", "PID", "UPTIME"
    );
    for row in rows {
        let pid = row
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "-".to_string());
        let uptime = row
            .uptime_seconds
            .map(|secs| format_duration_ms(secs * 1000))
            .unwrap_or_else(|| "-".to_string());
        let detail = if row.config_changed {
            format!("{} [config changed since start]", row.detail)
        } else {
            row.detail.clone()
        };
        println!(
            "{:<name_width$}  {:<status_width$}  {:<7}  {:<7}  {}",
            row.name, row.phase_display, pid, uptime, detail
        );
    }
    if rows.iter().all(|row| row.runner_pid == 0) {
        println!("(no agproc-managed service is running)");
    }
}
