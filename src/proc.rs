//! Process primitives: detached spawning, pseudo terminals, process groups.
//!
//! Why pseudo terminals: when a child's stdout is a pipe or a file, libc-based
//! runtimes (Python, some Node tooling) switch to *block* buffering, so the log
//! lines produced before the service is up may not be written at all until the buffer
//! fills or the process exits. With a tty on stdout they stay line buffered and
//! arrive immediately. Both stdout and stderr share one pty, preserving their
//! combined write order in the captured log.

use anyhow::{Context, Result, bail};
use nix::fcntl::{OFlag, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::openpty;
use nix::sys::signal::{Signal, killpg};
use nix::sys::termios::{self, InputFlags, LocalFlags, OutputFlags, SetArg};
use nix::unistd::{Pid, dup, setsid};
use std::collections::BTreeMap;
use std::os::fd::{AsFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, channel};

/// A pty pair with sane terminal settings for log capture.
pub struct Pty {
    pub master: OwnedFd,
    pub slave: OwnedFd,
}

pub fn open_pty() -> Result<Pty> {
    let size = nix::pty::Winsize {
        ws_row: 24,
        ws_col: 200,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pair = openpty(Some(&size), None).context("openpty failed")?;

    // The kernel would translate "\n" into "\r\n" on the slave side; disable
    // that (and echo/CR translation) so the captured bytes are byte-faithful.
    // `isatty()` stays true, which is what keeps the child line buffered.
    let mut term = termios::tcgetattr(&pair.slave).context("tcgetattr failed")?;
    term.output_flags.remove(OutputFlags::ONLCR);
    term.input_flags.remove(InputFlags::ICRNL);
    term.local_flags.remove(LocalFlags::ECHO);
    termios::tcsetattr(&pair.slave, SetArg::TCSANOW, &term).context("tcsetattr failed")?;

    set_nonblocking(&pair.master)?;
    Ok(Pty {
        master: pair.master,
        slave: pair.slave,
    })
}

pub fn set_nonblocking(fd: &OwnedFd) -> Result<()> {
    let flags = fcntl(fd.as_fd(), nix::fcntl::FcntlArg::F_GETFL).context("fcntl F_GETFL")?;
    let flags = OFlag::from_bits_truncate(flags);
    fcntl(
        fd.as_fd(),
        nix::fcntl::FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK),
    )
    .context("fcntl F_SETFL")?;
    Ok(())
}

/// Everything needed to launch one child.
pub struct SpawnSpec<'a> {
    pub argv: &'a [String],
    pub cwd: &'a Path,
    pub env: &'a BTreeMap<String, String>,
    pub output: RawFd,
}

/// Spawn a child with the same pty slave on stdout/stderr and `/dev/null` on
/// stdin.
///
/// The child gets its own process group (`setpgid(0, 0)`) inside the runner's
/// session. That is what makes `agproc stop` precise: `killpg(child_pgid)`
/// reaches the service and every descendant it spawned, without touching the
/// runner that has to record the outcome.
pub fn spawn(spec: &SpawnSpec<'_>) -> Result<Child> {
    let Some((program, rest)) = spec.argv.split_first() else {
        bail!("empty command");
    };
    if !spec.cwd.is_dir() {
        bail!(
            "working directory does not exist: {} (service cwd is relative to the project root)",
            spec.cwd.display()
        );
    }

    let mut cmd = Command::new(program);
    cmd.args(rest);
    cmd.current_dir(spec.cwd);
    // Reduce escape-sequence noise in captured logs; services can override.
    cmd.env("TERM", "dumb");
    cmd.env("NO_COLOR", "1");
    cmd.env("CLICOLOR", "0");
    cmd.env("CLICOLOR_FORCE", "0");
    for (key, value) in spec.env {
        cmd.env(key, value);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(dup_owned(spec.output)?));
    cmd.stderr(Stdio::from(dup_owned(spec.output)?));
    cmd.process_group(0);

    cmd.spawn()
        .with_context(|| format!("cannot spawn `{}`", spec.argv.join(" ")))
}

fn dup_owned(fd: RawFd) -> Result<OwnedFd> {
    // SAFETY: `fd` is a valid open descriptor owned by the caller.
    let owned = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    dup(owned)
        .map_err(anyhow::Error::from)
        .context("dup failed")
}

/// Launch a fully detached copy of agproc (used for the per-service runner).
///
/// `setsid` puts it in a new session and process group, so it survives the CLI
/// being killed and stays clear of a harness that kills the tool call's process
/// group. `stderr` joins the console log so early failures and panics are visible.
pub fn spawn_detached(program: &Path, args: &[String], stderr_log: Option<&Path>) -> Result<u32> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    match stderr_log {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("cannot open {}", path.display()))?;
            cmd.stderr(Stdio::from(file));
        }
        None => {
            cmd.stderr(Stdio::null());
        }
    }
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            setsid().map_err(std::io::Error::from)?;
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("cannot spawn {}", program.display()))?;
    Ok(child.id())
}

/// `kill` for a single process, tolerating one that is already gone.
pub fn signal_pid(pid: u32, signal: Signal) -> Result<()> {
    if pid == 0 {
        return Ok(());
    }
    match nix::sys::signal::kill(Pid::from_raw(pid as i32), signal) {
        Ok(()) => Ok(()),
        Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(err) => Err(anyhow::Error::from(err))
            .with_context(|| format!("cannot send {signal:?} to pid {pid}")),
    }
}

/// `killpg` that tolerates a group that is already gone.
pub fn signal_group(pgid: i32, signal: Signal) -> Result<()> {
    match killpg(Pid::from_raw(pgid), signal) {
        Ok(()) => Ok(()),
        Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(nix::errno::Errno::EPERM) if only_exited_members_left(pgid) => Ok(()),
        Err(err) => Err(anyhow::Error::from(err))
            .with_context(|| format!("cannot send {signal:?} to process group {pgid}")),
    }
}

/// Is there still a process in the group that has not exited? On macOS,
/// members committed to exit or waiting to be reaped do not count.
pub fn group_exists(pgid: i32) -> bool {
    match killpg(Pid::from_raw(pgid), None) {
        Ok(()) => true,
        Err(nix::errno::Errno::ESRCH) => false,
        Err(nix::errno::Errno::EPERM) => !only_exited_members_left(pgid),
        Err(_) => true,
    }
}

/// Is `killpg`'s EPERM merely a group whose members have committed to exit?
/// macOS can skip those members even before they become zombies, returning
/// EPERM until the parent (or launchd) has reaped them. Confirm native exit
/// status so these cleanup races do not hide actual permission errors.
#[cfg(target_os = "macos")]
fn only_exited_members_left(pgid: i32) -> bool {
    !crate::procinfo::group_has_running_member(pgid)
}

/// On Linux, EPERM always means a member the caller may not signal.
#[cfg(not(target_os = "macos"))]
fn only_exited_members_left(_pgid: i32) -> bool {
    false
}

/// Exit code of a finished child: signal deaths are reported as `128 + signal`.
pub fn exit_code_of(status: &ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// Wait for a child on a background thread so the caller can keep pumping logs.
pub struct ChildWatch {
    rx: Receiver<ExitStatus>,
    _handle: std::thread::JoinHandle<()>,
}

impl ChildWatch {
    pub fn spawn(mut child: Child) -> Self {
        let (tx, rx) = channel();
        let handle = std::thread::spawn(move || {
            let status = child.wait();
            let _ = tx.send(match status {
                Ok(status) => status,
                // A failed wait is reported as a synthetic failure; the state
                // file still records that the child is gone.
                Err(_) => ExitStatus::from_raw(1 << 8),
            });
        });
        Self {
            rx,
            _handle: handle,
        }
    }

    pub fn try_exit(&self) -> Option<ExitStatus> {
        self.rx.try_recv().ok()
    }
}

pub enum ReadOutcome {
    Data(Vec<u8>),
    Eof,
    WouldBlock,
}

/// Read whatever is available from a non-blocking pty master.
pub fn read_available(fd: &OwnedFd) -> ReadOutcome {
    let mut buf = [0u8; 8192];
    match nix::unistd::read(fd, &mut buf) {
        Ok(0) => ReadOutcome::Eof,
        Ok(n) => ReadOutcome::Data(buf[..n].to_vec()),
        Err(nix::errno::Errno::EAGAIN) => ReadOutcome::WouldBlock,
        // A signal (SIGTERM from `agproc stop`, for instance) interrupted the
        // read: report "nothing right now" and let the caller re-poll.
        Err(nix::errno::Errno::EINTR) => ReadOutcome::WouldBlock,
        // Linux reports EIO on a pty master once every slave is closed.
        Err(nix::errno::Errno::EIO) => ReadOutcome::Eof,
        Err(_) => ReadOutcome::Eof,
    }
}

/// Poll which of `fds` are readable within `timeout_ms`.
///
/// Signal interruptions are not errors here: they are exactly how `stop` gets
/// the runner's attention, so they surface as "nothing ready".
pub fn poll_readable(fds: &[RawFd], timeout_ms: u16) -> Result<Vec<bool>> {
    let mut pollfds: Vec<PollFd> = fds
        .iter()
        .map(|fd| {
            // SAFETY: the caller guarantees these descriptors stay open for the
            // duration of the call.
            PollFd::new(
                unsafe { std::os::fd::BorrowedFd::borrow_raw(*fd) },
                PollFlags::POLLIN,
            )
        })
        .collect();
    match poll(&mut pollfds, PollTimeout::from(timeout_ms)) {
        Ok(_) => {}
        Err(nix::errno::Errno::EINTR) => return Ok(vec![false; fds.len()]),
        Err(err) => return Err(anyhow::Error::from(err).context("poll failed")),
    }
    Ok(pollfds
        .iter()
        .map(|pfd| {
            pfd.revents()
                .map(|flags| flags.intersects(PollFlags::POLLIN | PollFlags::POLLHUP))
                .unwrap_or(false)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    #[test]
    fn shared_pty_is_line_buffered_and_preserves_write_order() {
        let pty = open_pty().unwrap();
        let argv = vec![
            "python3".to_string(),
            "-c".to_string(),
            r#"import os,sys,time
assert os.isatty(1) and os.isatty(2)
assert os.ttyname(1) == os.ttyname(2)
print('OUT-1')
sys.stderr.write('ERR-1\n')
sys.stderr.flush()
print('OUT-2')
time.sleep(5)
"#
            .to_string(),
        ];
        let env = BTreeMap::new();
        let mut child = spawn(&SpawnSpec {
            argv: &argv,
            cwd: Path::new("/tmp"),
            env: &env,
            output: pty.slave.as_raw_fd(),
        })
        .unwrap();

        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !out.ends_with(b"OUT-2\n") {
            let readable = poll_readable(&[pty.master.as_raw_fd()], 100).unwrap();
            if readable[0]
                && let ReadOutcome::Data(bytes) = read_available(&pty.master)
            {
                out.extend_from_slice(&bytes);
            }
        }

        signal_group(child.id() as i32, Signal::SIGKILL).unwrap();
        child.wait().unwrap();

        // Both fds are the same tty, output arrives before exit, and LF is kept.
        assert_eq!(out, b"OUT-1\nERR-1\nOUT-2\n", "combined bytes: {out:?}");
    }

    #[test]
    fn a_missing_group_does_not_exist_and_needs_no_signal() {
        let pgid = 0x7fff_fffe; // certainly no such group
        assert!(!group_exists(pgid));
        signal_group(pgid, Signal::SIGTERM).unwrap();
    }

    /// macOS answers EPERM, not success, when `killpg` finds only zombies.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_group_of_unreaped_zombies_is_gone_and_can_be_signalled() {
        use nix::libc;

        let mut child = Command::new("sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id() as i32;
        // WNOWAIT confirms an actual zombie without reaping it or relying on
        // sysinfo, which may omit zombie records on macOS.
        let deadline = Instant::now() + Duration::from_secs(5);
        let exited = loop {
            // SAFETY: siginfo is a valid output buffer, and WNOWAIT leaves
            // the direct child available for Child::wait below.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pgid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                )
            };
            assert_eq!(
                result,
                0,
                "waitid failed: {}",
                std::io::Error::last_os_error()
            );
            if info.si_pid == pgid && info.si_code == libc::CLD_EXITED {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        // The child has exited but is not reaped yet: only a zombie is left.
        let has_running_member = crate::procinfo::group_has_running_member(pgid);
        let exists = group_exists(pgid);
        let signalled = signal_group(pgid, Signal::SIGTERM);
        child.wait().unwrap();
        assert!(exited, "the child did not exit before the deadline");
        assert!(
            !has_running_member,
            "the native group query did not recognize the zombie"
        );
        assert!(!exists, "a group of zombies was reported as existing");
        signalled.unwrap();
        assert!(!group_exists(pgid));
    }

    #[test]
    #[allow(clippy::zombie_processes)] // the child is deliberately detached
    fn detached_child_survives_and_is_killable_by_group() {
        let mark = tempfile::tempdir().unwrap();
        let pid_file = mark.path().join("grandchild.pid");
        // A background grandchild in the same process group, exactly like a dev
        // server that spawns workers: killing the group must take it down too.
        let args = vec![
            "-c".to_string(),
            format!("sleep 30 & echo $! > {} ; wait", pid_file.display()),
        ];
        let pid = spawn_detached(Path::new("/bin/sh"), &args, None).unwrap();
        assert!(nix::sys::signal::kill(Pid::from_raw(pid as i32), None).is_ok());

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !pid_file.exists() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let grandchild: u32 = std::fs::read_to_string(&pid_file)
            .expect("grandchild pid file")
            .trim()
            .parse()
            .unwrap();
        assert!(crate::procinfo::pid_alive(grandchild));

        signal_group(pid as i32, Signal::SIGKILL).unwrap();
        // Reap our direct child so it does not linger as a zombie.
        let _ = nix::sys::wait::waitpid(Pid::from_raw(pid as i32), None);

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && crate::procinfo::pid_alive(grandchild) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !crate::procinfo::pid_alive(grandchild),
            "grandchild {grandchild} survived a process-group SIGKILL"
        );
    }
}
