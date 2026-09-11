//! Per-service log files: append, truncate, rotate, follow.
//!
//! A service has two destinations, and keeping them apart is the point:
//!
//! - the **run logs** under `.agproc/logs/` hold *only* run-cmd output, and are
//!   truncated when a session starts, so `agproc logs` always replays the last
//!   run-cmd and nothing else;
//! - the **console stream** under `.agproc/tmp/` is what `agproc start` and
//!   `agproc restart` forward live: agproc's own markers and the build-cmd
//!   output live there, transient by construction.
//!
//! Agproc never writes its own lines into the run logs.

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

use crate::paths::Project;

/// Every line agproc itself produces has this shape, so agents can parse it.
pub fn marker_line(text: &str) -> String {
    format!("===== {text} =====\n")
}

/// Write one log line to our own stdout or stderr, with an optional prefix.
pub fn emit(to_stderr: bool, prefix: &str, line: &[u8]) {
    let mut buffer = Vec::with_capacity(prefix.len() + line.len() + 1);
    buffer.extend_from_slice(prefix.as_bytes());
    buffer.extend_from_slice(line);
    buffer.push(b'\n');
    if to_stderr {
        let mut out = std::io::stderr().lock();
        let _ = out.write_all(&buffer);
        let _ = out.flush();
    } else {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(&buffer);
        let _ = out.flush();
    }
}

/// Splits a byte stream into lines, tolerant of non-UTF-8 output.
#[derive(Default)]
pub struct LineBuffer {
    buf: Vec<u8>,
}

impl LineBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push<F: FnMut(&[u8])>(&mut self, bytes: &[u8], mut emit: F) {
        self.buf.extend_from_slice(bytes);
        while let Some(index) = self.buf.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=index).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            emit(&line);
        }
    }

    /// Emit whatever has not been terminated by a newline yet.
    pub fn flush<F: FnMut(&[u8])>(&mut self, mut emit: F) {
        if self.buf.is_empty() {
            return;
        }
        let mut line = std::mem::take(&mut self.buf);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        emit(&line);
    }
}

/// Append-only writer for one service's destinations.
///
/// Two destinations exist per service:
///
/// - **run logs** (`logs/<svc>.stdout.log`, `logs/<svc>.stderr.log`): what
///   `agproc logs` replays. They hold run-cmd output only, and are truncated
///   when a session starts, so they always describe the last run-cmd.
/// - **console stream** (`tmp/<svc>.console.stdout`, `tmp/<svc>.console.stderr`):
///   what `agproc start`/`restart` forwards live. It also carries agproc's own
///   `===== ... =====` markers and the build-cmd output, which are therefore
///   kept out of the run logs. It lives in `tmp/` because it is transient.
pub struct LogSink {
    run: LogPair,
    console: LogPair,
    max_bytes: u64,
    write_run: bool,
    write_console: bool,
}

struct LogPair {
    out: SinkFile,
    err: SinkFile,
}

impl LogPair {
    fn open(out_path: &Path, err_path: &Path) -> Result<Self> {
        Ok(Self {
            out: SinkFile::open(out_path)?,
            err: SinkFile::open(err_path)?,
        })
    }

    fn truncate(&mut self) -> Result<()> {
        self.out.truncate()?;
        self.err.truncate()
    }
}

impl LogSink {
    /// Open both destinations and truncate them: a session always starts with
    /// empty logs, so nothing from a previous session can be mistaken for the
    /// current one.
    pub fn open(project: &Project, service: &str, max_bytes: u64) -> Result<Self> {
        let mut run = LogPair::open(&project.log_stdout(service), &project.log_stderr(service))?;
        let mut console =
            LogPair::open(&project.console_stdout(service), &project.console_stderr(service))?;
        run.truncate()?;
        console.truncate()?;
        Ok(Self {
            run,
            console,
            max_bytes,
            // The build phase writes to the console only: build output is not
            // part of the service's logs.
            write_run: false,
            write_console: true,
        })
    }

    /// The run-cmd is about to start: truncate the run logs and begin writing
    /// to them (that is what makes them "the last run-cmd's logs").
    pub fn begin_run(&mut self) -> Result<()> {
        self.run.truncate()?;
        self.write_run = true;
        Ok(())
    }

    /// Readiness has settled, so the CLI has stopped watching: stop duplicating
    /// run output into the console stream, which would otherwise grow for the
    /// whole lifetime of a long-running service. Markers keep being written.
    pub fn end_narrative(&mut self) {
        self.write_console = false;
    }

    pub fn marker(&mut self, text: &str) -> Result<()> {
        self.console
            .out
            .write(marker_line(text).as_bytes(), self.max_bytes)
    }

    pub fn write_stdout(&mut self, bytes: &[u8]) -> Result<()> {
        if self.write_run {
            self.run.out.write(bytes, self.max_bytes)?;
        }
        if self.write_console {
            self.console.out.write(bytes, self.max_bytes)?;
        }
        Ok(())
    }

    pub fn write_stderr(&mut self, bytes: &[u8]) -> Result<()> {
        if self.write_run {
            self.run.err.write(bytes, self.max_bytes)?;
        }
        if self.write_console {
            self.console.err.write(bytes, self.max_bytes)?;
        }
        Ok(())
    }
}

struct SinkFile {
    path: PathBuf,
    file: File,
    len: u64,
}

impl SinkFile {
    fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path: path.to_path_buf(),
            file,
            len,
        })
    }

    fn write(&mut self, bytes: &[u8], max_bytes: u64) -> Result<()> {
        if max_bytes > 0 && self.len > 0 && self.len + bytes.len() as u64 > max_bytes {
            self.rotate()?;
        }
        self.file
            .write_all(bytes)
            .with_context(|| format!("cannot write {}", self.path.display()))?;
        self.file.flush().ok();
        self.len += bytes.len() as u64;
        Ok(())
    }

    /// Empty the file in place (used at the start of a session, so a previous
    /// session's output can never be read as the current one).
    fn truncate(&mut self) -> Result<()> {
        self.file
            .set_len(0)
            .with_context(|| format!("cannot truncate {}", self.path.display()))?;
        self.len = 0;
        Ok(())
    }

    /// Move the current file to `<name>.1` (replacing any previous one) and
    /// continue in a fresh file. Followers detect this via the inode/size.
    fn rotate(&mut self) -> Result<()> {
        let mut rotated = self.path.clone().into_os_string();
        rotated.push(".1");
        let rotated = PathBuf::from(rotated);
        let _ = std::fs::remove_file(&rotated);
        std::fs::rename(&self.path, &rotated)
            .with_context(|| format!("cannot rotate {}", self.path.display()))?;
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("cannot reopen {}", self.path.display()))?;
        self.len = 0;
        Ok(())
    }
}

/// Reads new bytes appended to a log file, surviving rotation and truncation.
pub struct LogFollower {
    path: PathBuf,
    file: Option<File>,
    inode: u64,
    offset: u64,
}

impl LogFollower {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: None,
            inode: 0,
            offset: 0,
        }
    }

    /// Start reading at `offset` (clamped to the current end of file).
    pub fn seek_to(&mut self, offset: u64) -> Result<()> {
        self.file = None;
        self.inode = 0;
        self.offset = offset;
        self.open()?;
        self.offset = self.offset.min(self.path_len());
        Ok(())
    }

    fn path_len(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }

    fn open(&mut self) -> Result<()> {
        if self.file.is_some() {
            return Ok(());
        }
        match File::open(&self.path) {
            Ok(file) => {
                self.inode = file.metadata().map(|m| m.ino()).unwrap_or(0);
                self.file = Some(file);
                Ok(())
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("cannot open {}", self.path.display())),
        }
    }

    /// Bytes appended since the last call; empty when there is nothing new.
    ///
    /// Rotation and truncation are detected by re-reading the *path*, not the
    /// open handle: a rotated file keeps its old inode alive for readers.
    pub fn read_new(&mut self) -> Result<Vec<u8>> {
        let meta = match std::fs::metadata(&self.path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // The file is gone (a finished session cleans its console stream
                // up). Anything already buffered in our open handle is still
                // readable, so hand that over before letting go.
                let tail = self.drain_open_handle();
                self.file = None;
                self.inode = 0;
                self.offset = 0;
                return tail;
            }
            Err(err) => {
                return Err(err).with_context(|| format!("cannot stat {}", self.path.display()));
            }
        };

        if self.file.is_none() || self.inode != meta.ino() {
            self.file = Some(
                File::open(&self.path)
                    .with_context(|| format!("cannot open {}", self.path.display()))?,
            );
            if self.inode != 0 {
                // A different file is now at this path: start from its beginning.
                self.offset = 0;
            }
            self.inode = meta.ino();
        }
        if meta.len() < self.offset {
            self.offset = 0;
        }
        if meta.len() <= self.offset {
            return Ok(Vec::new());
        }

        let file = self.file.as_ref().expect("opened above");
        let mut buf = vec![0u8; (meta.len() - self.offset) as usize];
        let read = loop {
            match file.read_at(&mut buf, self.offset) {
                Ok(read) => break read,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("cannot read {}", self.path.display()));
                }
            }
        };
        buf.truncate(read);
        self.offset += read as u64;
        Ok(buf)
    }

    /// Read whatever is left in an already-open handle, for the case where the
    /// path disappeared underneath us (unlinked file).
    fn drain_open_handle(&mut self) -> Result<Vec<u8>> {
        let Some(file) = self.file.as_ref() else {
            return Ok(Vec::new());
        };
        let len = match file.metadata() {
            Ok(meta) => meta.len(),
            Err(_) => return Ok(Vec::new()),
        };
        if len <= self.offset {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; (len - self.offset) as usize];
        let read = match file.read_at(&mut buf, self.offset) {
            Ok(read) => read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => 0,
            Err(_) => 0,
        };
        buf.truncate(read);
        self.offset += read as u64;
        Ok(buf)
    }
}

/// Relays both log files of one service to our own stdout/stderr, preserving
/// the stream split and adding a prefix per line when several services are in
/// scope. Both streams share that prefix; they stay apart by destination.
/// Used by `agproc start` (live) and `agproc logs` (replay/follow).
pub struct LogRelay {
    out: LogFollower,
    err: LogFollower,
    out_lines: LineBuffer,
    err_lines: LineBuffer,
    prefix: String,
    enabled_out: bool,
    enabled_err: bool,
}

impl LogRelay {
    pub fn new(out_path: PathBuf, err_path: PathBuf, prefix: String) -> Self {
        Self {
            out: LogFollower::new(out_path),
            err: LogFollower::new(err_path),
            out_lines: LineBuffer::new(),
            err_lines: LineBuffer::new(),
            prefix,
            enabled_out: true,
            enabled_err: true,
        }
    }

    /// Restrict which streams are forwarded (used by `logs --stream`).
    pub fn enable(&mut self, stdout: bool, stderr: bool) {
        self.enabled_out = stdout;
        self.enabled_err = stderr;
    }

    pub fn seek(&mut self, stdout: u64, stderr: u64) -> Result<()> {
        self.out.seek_to(stdout)?;
        self.err.seek_to(stderr)
    }

    /// Forward everything appended since the last call.
    pub fn pump(&mut self) -> Result<bool> {
        let mut any = false;
        if self.enabled_out {
            let stdout = self.out.read_new()?;
            if !stdout.is_empty() {
                any = true;
                let prefix = self.prefix.clone();
                self.out_lines
                    .push(&stdout, |line| emit(false, &prefix, line));
            }
        }
        if self.enabled_err {
            let stderr = self.err.read_new()?;
            if !stderr.is_empty() {
                any = true;
                let prefix = self.prefix.clone();
                self.err_lines
                    .push(&stderr, |line| emit(true, &prefix, line));
            }
        }
        Ok(any)
    }

    /// Emit any trailing partial line (called once when forwarding stops).
    pub fn flush(&mut self) {
        let prefix = self.prefix.clone();
        self.out_lines.flush(|line| emit(false, &prefix, line));
        let prefix = self.prefix.clone();
        self.err_lines.flush(|line| emit(true, &prefix, line));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_buffer_splits_and_keeps_remainder() {
        let mut lines = Vec::new();
        let mut buffer = LineBuffer::new();
        buffer.push(b"one\ntwo\r\npart", |line| lines.push(line.to_vec()));
        assert_eq!(lines, vec![b"one".to_vec(), b"two".to_vec()]);
        buffer.flush(|line| lines.push(line.to_vec()));
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2], b"part".to_vec());
    }

    #[test]
    fn follower_reads_only_new_bytes_and_survives_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        std::fs::write(&path, b"first\n").unwrap();

        let mut follower = LogFollower::new(path.clone());
        follower.seek_to(0).unwrap();
        assert_eq!(follower.read_new().unwrap(), b"first\n");
        assert!(follower.read_new().unwrap().is_empty());

        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(b"second\n").unwrap();
        }
        assert_eq!(follower.read_new().unwrap(), b"second\n");

        // Rotation: the path now points at a fresh, shorter file.
        let rotated = dir.path().join("a.log.1");
        std::fs::rename(&path, &rotated).unwrap();
        std::fs::write(&path, b"fresh\n").unwrap();
        assert_eq!(follower.read_new().unwrap(), b"fresh\n");
    }

    #[test]
    fn sink_keeps_markers_and_build_output_out_of_the_run_logs() {
        let dir = tempfile::tempdir().unwrap();
        let project = crate::paths::Project {
            root: dir.path().to_path_buf(),
            config_path: dir.path().join("agproc.toml"),
        };
        let mut sink = LogSink::open(&project, "svc", 0).unwrap();

        // Build phase: markers and build output go to the console stream only.
        sink.marker("BUILDING").unwrap();
        sink.write_stdout(b"compiling\n").unwrap();
        sink.write_stderr(b"warning: unused import\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(project.console_stdout("svc")).unwrap(),
            "===== BUILDING =====\ncompiling\n"
        );
        assert_eq!(
            std::fs::read_to_string(project.console_stderr("svc")).unwrap(),
            "warning: unused import\n"
        );
        assert_eq!(
            std::fs::read_to_string(project.log_stdout("svc")).unwrap(),
            ""
        );

        // Run phase: run output lands in both destinations until readiness
        // settles, then only in the run logs.
        sink.begin_run().unwrap();
        sink.marker("RUNNING").unwrap();
        sink.write_stdout(b"listening\n").unwrap();
        sink.write_stderr(b"note\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(project.log_stdout("svc")).unwrap(),
            "listening\n"
        );
        assert_eq!(
            std::fs::read_to_string(project.log_stderr("svc")).unwrap(),
            "note\n"
        );
        assert_eq!(
            std::fs::read_to_string(project.console_stdout("svc")).unwrap(),
            "===== BUILDING =====\ncompiling\n===== RUNNING =====\nlistening\n"
        );

        sink.end_narrative();
        sink.marker("SERVICE EXITED (exit code 0)").unwrap();
        sink.write_stdout(b"later\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(project.log_stdout("svc")).unwrap(),
            "listening\nlater\n"
        );
        assert!(
            !std::fs::read_to_string(project.console_stdout("svc"))
                .unwrap()
                .contains("later"),
            "run output must stop being duplicated once readiness settled"
        );
    }

    #[test]
    fn a_new_session_starts_from_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        let project = crate::paths::Project {
            root: dir.path().to_path_buf(),
            config_path: dir.path().join("agproc.toml"),
        };
        {
            let mut sink = LogSink::open(&project, "svc", 0).unwrap();
            sink.marker("BUILDING").unwrap();
            sink.begin_run().unwrap();
            sink.write_stdout(b"previous run\n").unwrap();
        }
        let mut sink = LogSink::open(&project, "svc", 0).unwrap();
        sink.begin_run().unwrap();
        sink.write_stdout(b"current run\n").unwrap();

        let logs = std::fs::read_to_string(project.log_stdout("svc")).unwrap();
        assert_eq!(logs, "current run\n");
        let console = std::fs::read_to_string(project.console_stdout("svc")).unwrap();
        assert_eq!(console, "current run\n");
    }

    #[test]
    fn sink_rotates_when_the_cap_is_crossed() {
        let dir = tempfile::tempdir().unwrap();
        let project = crate::paths::Project {
            root: dir.path().to_path_buf(),
            config_path: dir.path().join("agproc.toml"),
        };
        let mut sink = LogSink::open(&project, "svc", 16).unwrap();
        sink.begin_run().unwrap();
        sink.write_stdout(b"0123456789\n").unwrap();
        sink.write_stdout(b"abcdefghij\n").unwrap();

        let rotated = dir.path().join(".agproc/logs/svc.stdout.log.1");
        assert!(rotated.exists());
        let current = std::fs::read_to_string(project.log_stdout("svc")).unwrap();
        assert_eq!(current, "abcdefghij\n");
    }

    #[test]
    fn follower_hands_over_bytes_of_an_unlinked_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gone.log");
        std::fs::write(&path, b"first\n").unwrap();

        let mut follower = LogFollower::new(path.clone());
        follower.seek_to(0).unwrap();
        assert_eq!(follower.read_new().unwrap(), b"first\n");

        // More output is written and then the file is removed underneath us.
        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(b"last words\n").unwrap();
        }
        std::fs::remove_file(&path).unwrap();
        assert_eq!(follower.read_new().unwrap(), b"last words\n");
        assert!(follower.read_new().unwrap().is_empty());
    }
}
