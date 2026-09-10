//! Local port ownership, used to defeat the nastiest false positive a
//! probe can produce: passing against a *foreign* listener while our
//! own `run-cmd` is already dead because it could not bind the port.
//!
//! Three mechanisms work together (see the plan): the preflight check records
//! who owned the port before `run-cmd` started, the post-probe verification
//! confirms the owner is still that same foreign process, and the runner's
//! independent `waitpid` makes a child exit always win over a probe result.

use crate::config::{ProbeTarget, Settings};
use crate::procinfo;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub pid: u32,
    pub comm: String,
}

impl Listener {
    pub fn describe(&self) -> String {
        let comm = if self.comm.is_empty() {
            "unknown"
        } else {
            self.comm.as_str()
        };
        format!("pid {} ({comm})", self.pid)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    /// Held by a process inside the service's own process group.
    Ours,
    /// Still held by the listener that was already there before `run-cmd`
    /// started: the probe passed against someone else's process.
    ConfirmedForeign(Listener),
    /// Someone else owns the port but was not there before `run-cmd` (a
    /// container runtime publishing on our behalf, for example). Warn only.
    Unexpected(Listener),
    /// Nothing could be determined: checks disabled, remote target, no local
    /// port, or nothing is listening.
    Unknown,
}

/// Snapshot of who holds the probe port, taken before `run-cmd` is started.
pub fn preflight(target: &ProbeTarget, settings: &Settings) -> Option<Listener> {
    if !settings.port_check {
        return None;
    }
    let port = target.local_port()?;
    procinfo::listeners_on_port(port)
        .into_iter()
        .next()
        .map(|(pid, comm)| Listener { pid, comm })
}

/// Who owns the probe port now that the probe succeeded?
pub fn verify(
    target: &ProbeTarget,
    settings: &Settings,
    child_pgid: i32,
    preflight: Option<&Listener>,
) -> Ownership {
    if !settings.port_check {
        return Ownership::Unknown;
    }
    let Some(port) = target.local_port() else {
        return Ownership::Unknown;
    };
    let listeners = procinfo::listeners_on_port(port);
    if listeners.is_empty() {
        return Ownership::Unknown;
    }
    if listeners
        .iter()
        .any(|(pid, _)| procinfo::pid_pgrp(*pid) == Some(child_pgid))
    {
        return Ownership::Ours;
    }
    let to_listener = |(pid, comm): (u32, String)| Listener { pid, comm };
    if let Some(before) = preflight
        && let Some(mine) = listeners.iter().find(|(pid, _)| *pid == before.pid)
    {
        return Ownership::ConfirmedForeign(to_listener(mine.clone()));
    }
    Ownership::Unexpected(to_listener(listeners.into_iter().next().expect("non-empty")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn settings(port_check: bool) -> Settings {
        Settings {
            port_check,
            ..Settings::default()
        }
    }

    fn http_target(port: u16) -> ProbeTarget {
        ProbeTarget::Http {
            scheme: "http".to_string(),
            host: "127.0.0.1".to_string(),
            port,
            path: "/".to_string(),
        }
    }

    #[test]
    fn finds_our_own_listener_as_ours() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // The test process owns the listener, so its own group must match.
        let our_group = procinfo::pid_pgrp(std::process::id()).unwrap();
        let ownership = verify(&http_target(port), &settings(true), our_group, None);
        assert_eq!(ownership, Ownership::Ours);
        drop(listener);
    }

    #[test]
    fn detects_a_confirmed_foreign_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let before = preflight(&http_target(port), &settings(true)).expect("preflight sees it");
        assert_eq!(before.pid, std::process::id());

        // A child group that does not contain the listener's process.
        let ownership = verify(&http_target(port), &settings(true), 0x7fff_fffe, Some(&before));
        assert_eq!(ownership, Ownership::ConfirmedForeign(before));
        drop(listener);
    }

    #[test]
    fn unexpected_owner_is_not_confirmed_foreign() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let ownership = verify(&http_target(port), &settings(true), 0x7fff_fffe, None);
        match ownership {
            Ownership::Unexpected(l) => assert_eq!(l.pid, std::process::id()),
            other => panic!("expected Unexpected, got {other:?}"),
        }
        drop(listener);
    }

    #[test]
    fn free_port_is_unknown_and_checks_can_be_disabled() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert_eq!(
            verify(&http_target(port), &settings(true), 1, None),
            Ownership::Unknown
        );
        assert_eq!(
            preflight(&http_target(port), &settings(false)),
            None,
            "disabled checks must not snapshot"
        );

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(
            verify(&http_target(port), &settings(false), 0, None),
            Ownership::Unknown
        );
    }

    #[test]
    fn remote_targets_are_not_attributed() {
        let target = ProbeTarget::Http {
            scheme: "http".to_string(),
            host: "example.com".to_string(),
            port: 80,
            path: "/".to_string(),
        };
        assert_eq!(preflight(&target, &settings(true)), None);
        assert_eq!(verify(&target, &settings(true), 1, None), Ownership::Unknown);
    }

    #[test]
    fn no_probe_means_no_port_checks() {
        assert_eq!(preflight(&ProbeTarget::None, &settings(true)), None);
        assert_eq!(
            verify(&ProbeTarget::None, &settings(true), 1, None),
            Ownership::Unknown
        );
    }
}
