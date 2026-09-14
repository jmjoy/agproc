//! `agproc logs` — replay and follow the output of a service's run-cmd.
//!
//! The run logs are truncated when a session starts, so the whole file is
//! exactly the last run-cmd's output: no agproc markers, no build output, no
//! earlier sessions. The two streams are replayed independently and keep their
//! original destinations.

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::cli::Failure;
use crate::cmd::{Prefix, prefix_width, select_services};
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
    let width = prefix_width(services.iter().map(|service| service.name.as_str()));

    if request.follow {
        install_interrupt_handler();
        follow(project, &services, &request, width)?;
        return Ok(exit::OK);
    }

    let mut printed_anything = false;
    for service in &services {
        let prefix = Prefix::service(&service.name, width);
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
            let prefix_text = prefix.label.clone();
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

/// The byte offset at which a follower should start to replay a tail window.
///
/// A partial final line counts toward the window, matching non-follow replay.
/// `LogRelay` still buffers that partial line until it is completed or flushed.
fn tail_start_offset(bytes: &[u8], tail: usize) -> usize {
    if tail == 0 {
        return bytes.len();
    }

    let mut line_count = bytes.iter().filter(|&&byte| byte == b'\n').count();
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        line_count += 1;
    }
    if line_count <= tail {
        return 0;
    }

    let discarded = line_count - tail;
    bytes
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'\n')
        .nth(discarded - 1)
        .map_or(0, |(index, _)| index + 1)
}

/// Determine a follower's initial offset from one file snapshot.
///
/// Missing logs are equivalent to empty logs; other I/O failures stay visible
/// to the caller rather than silently turning a broken log path into no output.
fn follow_start_offset(path: &Path, tail: Option<usize>) -> Result<u64> {
    let Some(tail) = tail else {
        return Ok(0);
    };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
    };
    Ok(tail_start_offset(&bytes, tail) as u64)
}

fn follow(
    project: &Project,
    services: &[&crate::config::Service],
    request: &Request,
    width: Option<usize>,
) -> Result<(), Failure> {
    let mut relays: Vec<(String, LogRelay)> = Vec::new();
    for service in services {
        let prefix = Prefix::service(&service.name, width);
        let stdout_offset = follow_start_offset(&project.log_stdout(&service.name), request.tail)
            .map_err(Failure::from)?;
        let stderr_offset = follow_start_offset(&project.log_stderr(&service.name), request.tail)
            .map_err(Failure::from)?;
        let mut relay = LogRelay::new(
            project.log_stdout(&service.name),
            project.log_stderr(&service.name),
            prefix.label.clone(),
        );
        relay.enable(
            !matches!(request.stream, Stream::Stderr),
            !matches!(request.stream, Stream::Stdout),
        );
        relay
            .seek(stdout_offset, stderr_offset)
            .map_err(Failure::from)?;
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

#[cfg(test)]
mod tests {
    use super::{follow_start_offset, tail_start_offset};

    #[test]
    fn tail_start_offset_keeps_the_requested_logical_lines() {
        let cases: &[(&str, &[u8], usize, usize)] = &[
            ("empty", b"", 3, 0),
            ("zero", b"one\ntwo\n", 0, 8),
            ("fewer than requested", b"one\ntwo\n", 3, 0),
            ("exactly requested", b"one\ntwo\n", 2, 0),
            ("complete lines", b"one\ntwo\nthree\n", 2, 4),
            ("blank line", b"one\n\ntwo\n", 1, 5),
            ("crlf", b"one\r\ntwo\r\nthree\r\n", 2, 5),
            ("partial final line", b"one\ntwo", 1, 4),
        ];

        for &(name, bytes, tail, expected) in cases {
            assert_eq!(tail_start_offset(bytes, tail), expected, "{name}");
        }
    }

    #[test]
    fn follow_start_offset_treats_a_missing_log_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            follow_start_offset(&dir.path().join("missing.log"), Some(5)).unwrap(),
            0
        );
    }
}
