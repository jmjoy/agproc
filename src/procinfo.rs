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
    pid_status(pid).is_some_and(|status| status != ProcessStatus::Zombie)
}

fn pid_status(pid: u32) -> Option<ProcessStatus> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    let pid = sysinfo::Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().without_tasks(),
    );
    system.process(pid).map(|process| process.status())
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

/// Does the process group still have a member that has not committed to exit?
///
/// macOS needs this because `killpg` answers EPERM, not success, for a group
/// whose members have exited or are exiting, which looks like a group the
/// caller may not signal. Failed queries and unknown statuses count as running.
#[cfg(target_os = "macos")]
pub fn group_has_running_member(pgid: i32) -> bool {
    use nix::libc;

    group_has_running_member_with(
        |pids| {
            let buffer = if pids.is_empty() {
                std::ptr::null_mut()
            } else {
                pids.as_mut_ptr().cast()
            };
            // SAFETY: `pids` is a valid buffer; the helper checks that its byte
            // size fits c_int. A null buffer requests the capacity in PIDs.
            unsafe {
                libc::proc_listpgrppids(pgid, buffer, std::mem::size_of_val(pids) as libc::c_int)
            }
        },
        group_member_is_running,
    )
}

/// Query a group member including zombie and in-exit records. `sysinfo` may
/// omit these while `kill(pid, 0)` still succeeds, so neither can confirm exit.
#[cfg(target_os = "macos")]
fn group_member_is_running(pid: u32) -> Option<bool> {
    use nix::{errno::Errno, libc};
    use std::mem::{MaybeUninit, size_of};

    // Defined in macOS <sys/proc_info.h>, but not exposed by libc. The kernel
    // sets this when exit is committed, before p_stat transitions to SZOMB.
    const PROC_FLAG_INEXIT: u32 = 4;

    let pid = i32::try_from(pid).ok().filter(|pid| *pid > 0)?;
    let mut info = MaybeUninit::<libc::proc_bsdshortinfo>::uninit();
    let size = size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
    Errno::clear();
    // SAFETY: the buffer is aligned and sized for PROC_PIDT_SHORTBSDINFO and
    // only read after a complete write. A nonzero arg explicitly includes
    // zombies; the default arg of zero would return ESRCH for them.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            1,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written == size {
        // SAFETY: proc_pidinfo initialized the full struct above.
        let info = unsafe { info.assume_init() };
        Some(info.pbsi_status != libc::SZOMB && info.pbsi_flags & PROC_FLAG_INEXIT == 0)
    } else if written <= 0 && Errno::last() == Errno::ESRCH {
        // The member was reaped between listing the group and inspecting it.
        Some(false)
    } else {
        // Permission errors and incomplete reads cannot confirm an exit.
        None
    }
}

// Keep the snapshot logic testable on Linux as well as macOS. The callbacks
// provide the native PID enumeration and a member's running/exited/unknown state.
#[cfg(any(target_os = "macos", test))]
fn group_has_running_member_with(
    mut list_pids: impl FnMut(&mut [i32]) -> i32,
    mut is_running: impl FnMut(u32) -> Option<bool>,
) -> bool {
    use nix::errno::Errno;
    use std::mem::size_of;

    // Both calls count PIDs, not bytes; an empty slice requests the capacity.
    Errno::clear();
    let Some(capacity) = usize::try_from(list_pids(&mut []))
        .ok()
        .filter(|count| *count > 0 && *count <= i32::MAX as usize / size_of::<i32>())
    else {
        return true;
    };
    let mut pids = vec![0; capacity];
    // libproc returns zero both for an empty group and on syscall failure.
    // Clear the original killpg EPERM so errno distinguishes those outcomes.
    Errno::clear();
    let listed = list_pids(&mut pids);
    let error = Errno::last_raw();
    // A completely filled buffer may have been cut short. Neither a failed
    // query nor an incomplete snapshot can establish that all members exited.
    let Some(listed) = usize::try_from(listed)
        .ok()
        .filter(|count| *count < capacity && (*count != 0 || error == 0))
    else {
        return true;
    };
    pids[..listed].iter().any(|pid| {
        u32::try_from(*pid)
            .ok()
            .filter(|pid| *pid > 0)
            .and_then(&mut is_running)
            != Some(false)
    })
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
    fn failed_group_enumeration_does_not_confirm_that_members_exited() {
        use nix::errno::Errno;

        for error in [Errno::EPERM, Errno::EIO] {
            let running = group_has_running_member_with(
                |pids| {
                    if pids.is_empty() {
                        4
                    } else {
                        error.set();
                        0 // libproc's failure return value
                    }
                },
                |_| panic!("failed enumeration must not inspect members"),
            );
            assert!(running, "{error} must not be mistaken for an empty group");
        }
    }

    #[test]
    fn an_empty_group_is_gone_even_after_an_earlier_errno() {
        use nix::errno::Errno;

        Errno::EPERM.set(); // the killpg failure that triggered the lookup
        let running = group_has_running_member_with(
            |pids| {
                if pids.is_empty() {
                    // Even a successful capacity query may leave errno set.
                    Errno::EIO.set();
                    4
                } else {
                    0 // success: no members remain and errno is untouched
                }
            },
            |_| panic!("an empty group has no members to inspect"),
        );
        assert!(!running);
    }

    #[test]
    fn only_confirmed_exited_members_allow_a_group_to_count_as_gone() {
        for (states, expected) in [
            ([Some(false), Some(false)], false),
            ([Some(false), Some(true)], true),
            ([Some(false), None], true),
        ] {
            let running = group_has_running_member_with(
                |pids| {
                    if pids.is_empty() {
                        4
                    } else {
                        pids[..2].copy_from_slice(&[1, 2]);
                        2
                    }
                },
                |pid| states[pid as usize - 1],
            );
            assert_eq!(running, expected, "member states: {states:?}");
        }
    }

    #[test]
    fn failed_or_truncated_group_snapshots_count_as_running() {
        for (capacity, listed) in [(0, 0), (-1, 0), (i32::MAX, 0), (4, -1), (4, 4), (4, 5)] {
            let running = group_has_running_member_with(
                |pids| if pids.is_empty() { capacity } else { listed },
                |_| panic!("an incomplete snapshot must not inspect members"),
            );
            assert!(running, "capacity {capacity}, listed {listed}");
        }
    }

    #[test]
    fn invalid_group_members_cannot_confirm_that_the_group_exited() {
        for pid in [0, -1] {
            let running = group_has_running_member_with(
                |pids| {
                    if pids.is_empty() {
                        4
                    } else {
                        pids[0] = pid;
                        1
                    }
                },
                |_| panic!("invalid PIDs must not be inspected"),
            );
            assert!(running, "invalid pid {pid}");
        }
    }

    #[test]
    fn current_process_has_a_stable_identity_and_group() {
        let pid = std::process::id();
        assert!(pid_alive(pid));
        let start = pid_start_ticks(pid).expect("read our own start time");
        assert_ne!(start, 0);
        assert_eq!(pid_start_ticks(pid), Some(start));
        assert_eq!(pid_pgrp(pid), Some(nix::unistd::getpgrp().as_raw()));
        #[cfg(target_os = "macos")]
        {
            assert_eq!(group_member_is_running(pid), Some(true));
            assert!(group_has_running_member(nix::unistd::getpgrp().as_raw()));
        }
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
