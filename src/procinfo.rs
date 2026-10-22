//! Process identity and TCP listener ownership on Linux and macOS.
//!
//! `sysinfo` and `listeners` handle platform-specific process/socket discovery;
//! `nix` provides process groups. Only the high-resolution start time (`sysinfo`
//! exposes whole seconds) and, on macOS, the members of a process group need a
//! small native implementation.

use nix::unistd::{Pid, getpgid};
use sysinfo::{ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};

/// Alive means "exists and is not a zombie". Refresh only the requested process,
/// without collecting its threads, memory, CPU usage, or environment.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    let pid = sysinfo::Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().without_tasks(),
    );
    system
        .process(pid)
        .is_some_and(|process| process.status() != ProcessStatus::Zombie)
}

/// An opaque, high-resolution process start time used to detect PID reuse.
///
/// Linux uses clock ticks since boot, preserving existing `runner-start-ticks`
/// records. macOS uses microseconds since the Unix epoch. Neither value is a
/// wall-clock timestamp for display, and values are compared only on one host.
#[cfg(target_os = "linux")]
pub fn pid_start_ticks(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `comm` can contain spaces and parentheses, so split after its last `)`.
    // starttime is field 22 overall, index 19 after comm.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// See the Linux implementation for the identity contract.
#[cfg(target_os = "macos")]
pub fn pid_start_ticks(pid: u32) -> Option<u64> {
    use nix::libc;
    use std::mem::{MaybeUninit, size_of};

    let pid = i32::try_from(pid).ok().filter(|pid| *pid > 0)?;
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is correctly aligned and sized for PROC_PIDTBSDINFO.
    // It is read only when proc_pidinfo reports that it filled the whole buffer.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: proc_pidinfo initialized the complete proc_bsdinfo above.
    let info = unsafe { info.assume_init() };
    info.pbi_start_tvsec
        .checked_mul(1_000_000)?
        .checked_add(info.pbi_start_tvusec)
}

/// Process group id. PID zero must not be interpreted as "the current process".
pub fn pid_pgrp(pid: u32) -> Option<i32> {
    let pid = i32::try_from(pid).ok().filter(|pid| *pid > 0)?;
    getpgid(Some(Pid::from_raw(pid))).ok().map(Pid::as_raw)
}

/// Does the process group still have a member that is running, that is, one
/// that is not a zombie waiting to be reaped?
///
/// macOS needs this because `killpg` answers EPERM, not success, for a group
/// whose members are all zombies, which looks like a group the caller may not
/// signal. When the members cannot be listed the group counts as running.
#[cfg(target_os = "macos")]
pub fn group_has_running_member(pgid: i32) -> bool {
    use nix::libc;
    use std::mem::size_of;

    // Both calls count pids, not bytes. A null buffer asks for the number of
    // pids to make room for, which includes some slack for new processes.
    // SAFETY: with a null buffer nothing is written.
    let capacity = unsafe { libc::proc_listpgrppids(pgid, std::ptr::null_mut(), 0) };
    let Some(capacity) = usize::try_from(capacity).ok().filter(|count| *count > 0) else {
        return true;
    };
    let Ok(size) = libc::c_int::try_from(capacity * size_of::<libc::pid_t>()) else {
        return true;
    };
    let mut pids: Vec<libc::pid_t> = vec![0; capacity];
    // SAFETY: `pids` is valid for writes of `size` bytes.
    let listed = unsafe { libc::proc_listpgrppids(pgid, pids.as_mut_ptr().cast(), size) };
    // A completely filled buffer may have been cut short.
    let Some(listed) = usize::try_from(listed)
        .ok()
        .filter(|count| *count < capacity)
    else {
        return true;
    };
    pids[..listed]
        .iter()
        .any(|pid| u32::try_from(*pid).is_ok_and(pid_alive))
}

/// `(pid, comm)` pairs currently listening on a local TCP port.
///
/// Discovery is best effort when OS permissions prevent inspecting a process.
/// Connected TCP sockets and UDP sockets must never count as TCP listeners.
pub fn listeners_on_port(port: u16) -> Vec<(u32, String)> {
    let mut owners: Vec<_> = listeners::get_all()
        .unwrap_or_default()
        .into_iter()
        .filter(|listener| {
            listener.protocol == listeners::Protocol::TCP
                && listener.state == listeners::SocketState::Listen
                && listener.socket.port() == port
        })
        .map(|listener| (listener.process.pid, listener.process.name))
        .collect();
    owners.sort_unstable();
    owners.dedup_by_key(|(pid, _)| *pid);
    owners
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream, UdpSocket};
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn current_process_has_a_stable_identity_and_group() {
        let pid = std::process::id();
        assert!(pid_alive(pid));
        let start = pid_start_ticks(pid).expect("read our own start time");
        assert_ne!(start, 0);
        assert_eq!(pid_start_ticks(pid), Some(start));
        assert_eq!(pid_pgrp(pid), Some(nix::unistd::getpgrp().as_raw()));
    }

    #[test]
    fn invalid_pids_do_not_refer_to_the_current_process() {
        for pid in [0, i32::MAX as u32, u32::MAX] {
            assert!(!pid_alive(pid));
            assert_eq!(pid_start_ticks(pid), None);
            assert_eq!(pid_pgrp(pid), None);
        }
    }

    #[test]
    fn an_exited_child_is_not_alive_even_before_it_is_reaped() {
        let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut alive = pid_alive(child.id());
        while alive && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            alive = pid_alive(child.id());
        }
        // Reap even if the assertion fails, so a failure does not leave a zombie.
        assert!(child.wait().unwrap().success());
        assert!(!alive, "an exited, unreaped child was reported as alive");
    }

    #[test]
    fn finds_ipv6_listeners_and_deduplicates_the_owner() {
        let ipv6 = TcpListener::bind("[::1]:0").unwrap();
        let port = ipv6.local_addr().unwrap().port();
        let ipv4 = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
        let owners = listeners_on_port(port);
        assert_eq!(owners.len(), 1, "owners: {owners:?}");
        assert_eq!(owners[0].0, std::process::id());
        assert!(!owners[0].1.is_empty());
        drop((ipv4, ipv6));
    }

    #[test]
    fn connected_tcp_and_udp_sockets_are_not_listeners() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(server.local_addr().unwrap()).unwrap();
        let (accepted, _) = server.accept().unwrap();
        let client_addr = client.local_addr().unwrap();
        let udp = UdpSocket::bind(client_addr).unwrap();
        assert!(listeners_on_port(client_addr.port()).is_empty());
        drop((udp, accepted, client, server));
    }
}
