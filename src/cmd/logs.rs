//! `agproc logs` — replay and follow the output of a service's run-cmd.
//!
//! The run logs are truncated when a session starts, so the whole file is
//! exactly the last run-cmd's output: no agproc markers, no build output, no
//! earlier sessions. The two streams are replayed independently and keep their
//! original destinations.

use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::cli::Failure;
use crate::cmd::{Prefix, select_services};
use crate::config::Config;
use crate::exit;
use crate::logstore::{LineBuffer, LogFollower, LogRelay, emit};
use crate::paths::Project;
use crate::state;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Both,
    Stdout,
    Stderr,
}

pub struct Request {
    pub services: Vec<String>,
    pub tail: Option<usize>,
    pub follow: bool,
    pub stream: Stream,
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_interrupt(_signal: nix::libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

fn install_interrupt_handler() {
    let action = nix::sys::signal::SigAction::new(
        nix::sys::signal::SigHandler::Handler(on_interrupt),
        nix::sys::signal::SaFlags::empty(),
        nix::sys::signal::SigSet::empty(),
    );
    // SAFETY: the handler only performs an atomic store.
    unsafe {
        let _ = nix::sys::signal::sigaction(nix::sys::signal::Signal::SIGINT, &action);
        let _ = nix::sys::signal::sigaction(nix::sys::signal::Signal::SIGTERM, &action);
    }
}

pub fn run(project: &Project, config: &Config, request: Request) -> Result<i32, Failure> {
    let services = select_services(config, &request.services).map_err(Failure::config)?;
    if let Some(warning) = project.layout_warning() {
        Prefix::plain().marker(&format!("WARNING: {warning}"));
    }
    let multi = services.len() > 1;

    if request.follow {
        install_interrupt_handler();
        follow(project, &services, &request, multi)?;
        return Ok(exit::OK);
    }

    let mut printed_anything = false;
    for service in &services {
        let prefix = Prefix::service(&service.name, multi);
        let mut chunks: Vec<(bool, Vec<u8>)> = Vec::new();
        if matches!(request.stream, Stream::Both | Stream::Stdout) {
            let mut follower = LogFollower::new(project.log_stdout(&service.name));
            follower.seek_to(0).map_err(Failure::from)?;
            chunks.push((false, follower.read_new().map_err(Failure::from)?));
        }
        if matches!(request.stream, Stream::Both | Stream::Stderr) {
            let mut follower = LogFollower::new(project.log_stderr(&service.name));
            follower.seek_to(0).map_err(Failure::from)?;
            chunks.push((true, follower.read_new().map_err(Failure::from)?));
        }

        for (is_stderr, bytes) in chunks {
            if bytes.is_empty() {
                continue;
            }
            printed_anything = true;
            let mut lines: Vec<Vec<u8>> = Vec::new();
            let mut buffer = LineBuffer::new();
            buffer.push(&bytes, |line| lines.push(line.to_vec()));
            buffer.flush(|line| lines.push(line.to_vec()));
            if let Some(tail) = request.tail {
                if tail == 0 {
                    lines.clear();
                } else if lines.len() > tail {
                    lines.drain(..lines.len() - tail);
                }
            }
            let prefix_text = if is_stderr {
                prefix.err.clone()
            } else {
                prefix.out.clone()
            };
            for line in lines {
                emit(is_stderr, &prefix_text, &line);
            }
        }
    }

    if !printed_anything {
        Prefix::plain().marker("NO LOGS YET");
    }
    Ok(exit::OK)
}

fn follow(
    project: &Project,
    services: &[&crate::config::Service],
    request: &Request,
    multi: bool,
) -> Result<(), Failure> {
    let mut relays: Vec<(String, LogRelay)> = Vec::new();
    for service in services {
        let prefix = Prefix::service(&service.name, multi);
        let mut relay = LogRelay::new(
            project.log_stdout(&service.name),
            project.log_stderr(&service.name),
            prefix.out.clone(),
            prefix.err.clone(),
        );
        relay.enable(
            !matches!(request.stream, Stream::Stderr),
            !matches!(request.stream, Stream::Stdout),
        );
        relay.seek(0, 0).map_err(Failure::from)?;
        relays.push((service.name.clone(), relay));
    }

    loop {
        for (_, relay) in relays.iter_mut() {
            relay.pump().map_err(Failure::from)?;
        }
        if INTERRUPTED.load(Ordering::SeqCst) {
            break;
        }
        // Stop following once nothing is running any more: an agent should not
        // have to time out a `logs -f` call.
        let all_gone = services.iter().all(|service| {
            !state::load(&project.state_path(&service.name))
                .state
                .map(|state| state.runner_live())
                .unwrap_or(false)
        });
        if all_gone {
            for (_, relay) in relays.iter_mut() {
                let _ = relay.pump();
                relay.flush();
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    for (_, relay) in relays.iter_mut() {
        relay.flush();
    }
    Ok(())
}
