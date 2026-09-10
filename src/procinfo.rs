//! `/proc` helpers. Linux only, which is all agproc targets.

use std::path::PathBuf;

fn proc_path(pid: u32, leaf: &str) -> PathBuf {
    PathBuf::from("/proc").join(pid.to_string()).join(leaf)
}

/// `state` field of `/proc/<pid>/stat`, e.g. `R`, `S`, `Z`.
pub fn pid_state(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(proc_path(pid, "stat")).ok()?;
    let after = stat.rsplit_once(')')?.1;
    after.split_whitespace().next()?.chars().next()
}

/// Alive means "exists and is not a zombie".
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    match pid_state(pid) {
        Some('Z') | None => false,
        Some(_) => true,
    }
}

/// Fields of `/proc/<pid>/stat` after the `comm` field, which may itself
/// contain spaces and parentheses; splitting on the last `)` is the documented
/// safe way to parse it.
fn stat_tail(pid: u32) -> Option<Vec<String>> {
    let stat = std::fs::read_to_string(proc_path(pid, "stat")).ok()?;
    let after = stat.rsplit_once(')')?.1;
    Some(after.split_whitespace().map(str::to_string).collect())
}

/// `starttime` (field 22 overall, index 19 after `comm`), in clock ticks since
/// boot. Compared against a recorded value it detects PID reuse.
pub fn pid_start_ticks(pid: u32) -> Option<u64> {
    stat_tail(pid)?.get(19)?.parse().ok()
}

/// Process group id (field 5 overall, index 2 after `comm`).
pub fn pid_pgrp(pid: u32) -> Option<i32> {
    stat_tail(pid)?.get(2)?.parse().ok()
}

/// Short command name from `/proc/<pid>/comm`, for diagnostics.
pub fn pid_comm(pid: u32) -> String {
    std::fs::read_to_string(proc_path(pid, "comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Inodes of sockets in LISTEN state bound to `port`, from `/proc/net/tcp` and
/// `/proc/net/tcp6` (IPv6 entries cover IPv4-mapped listeners too).
pub fn listener_inodes_for_port(port: u16) -> Vec<u64> {
    let mut inodes = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(content) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in content.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 {
                continue;
            }
            if fields[3] != "0A" {
                continue; // 0A = TCP_LISTEN
            }
            let Some((_, local_port)) = fields[1].rsplit_once(':') else {
                continue;
            };
            let Ok(local_port) = u16::from_str_radix(local_port, 16) else {
                continue;
            };
            if local_port != port {
                continue;
            }
            if let Ok(inode) = fields[9].parse::<u64>() {
                inodes.push(inode);
            }
        }
    }
    inodes.sort_unstable();
    inodes.dedup();
    inodes
}

/// PIDs holding any of `inodes`, by scanning `/proc/<pid>/fd/*`.
///
/// This is the only way to map a socket inode to a process without shelling out
/// to `ss`/`lsof`, which agproc deliberately does not depend on.
pub fn pids_for_inodes(inodes: &[u64]) -> Vec<u32> {
    if inodes.is_empty() {
        return Vec::new();
    }
    let wanted: Vec<String> = inodes.iter().map(|i| format!("socket:[{i}]")).collect();
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            if wanted.iter().any(|w| target.to_string_lossy() == w.as_str()) {
                pids.push(pid);
                break;
            }
        }
    }
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// `(pid, comm)` pairs currently listening on a local port.
pub fn listeners_on_port(port: u16) -> Vec<(u32, String)> {
    let inodes = listener_inodes_for_port(port);
    pids_for_inodes(&inodes)
        .into_iter()
        .map(|pid| (pid, pid_comm(pid)))
        .collect()
}
