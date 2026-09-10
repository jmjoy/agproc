//! Per-service log files: append, rotate, follow.
//!
//! The log files are the single source of truth for what a service printed:
//! the runner writes them, the CLI tails them for live output, and `agproc logs`
//! replays them later. stdout and stderr live in separate files so the two
//! streams can be re-emitted (and filtered) independently.

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

/// Append-only writer for one service's two log files.
pub struct LogSink {
    stdout: SinkFile,
    stderr: SinkFile,
    max_bytes: u64,
}

impl LogSink {
    pub fn open(project: &Project, service: &str, max_bytes: u64) -> Result<Self> {
        Ok(Self {
            stdout: SinkFile::open(&project.log_stdout(service))?,
            stderr: SinkFile::open(&project.log_stderr(service))?,
            max_bytes,
        })
    }

    /// Current end offsets, recorded as the start of this session.
    pub fn offsets(&self) -> (u64, u64) {
        (self.stdout.len, self.stderr.len)
    }

    pub fn marker(&mut self, text: &str) -> Result<()> {
        self.stdout.write(marker_line(text).as_bytes(), self.max_bytes)
    }

    pub fn write_stdout(&mut self, bytes: &[u8]) -> Result<()> {
        self.stdout.write(bytes, self.max_bytes)
    }

    pub fn write_stderr(&mut self, bytes: &[u8]) -> Result<()> {
        self.stderr.write(bytes, self.max_bytes)
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
                self.file = None;
                self.inode = 0;
                self.offset = 0;
                return Ok(Vec::new());
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
}

/// Relays both log files of one service to our own stdout/stderr, preserving
/// the stream split and adding a prefix per line when several services are in
/// scope. Used by `agproc start` (live) and `agproc logs` (replay/follow).
pub struct LogRelay {
    out: LogFollower,
    err: LogFollower,
    out_lines: LineBuffer,
    err_lines: LineBuffer,
    pub prefix_out: String,
    pub prefix_err: String,
    enabled_out: bool,
    enabled_err: bool,
}

impl LogRelay {
    pub fn new(out_path: PathBuf, err_path: PathBuf, prefix_out: String, prefix_err: String) -> Self {
        Self {
            out: LogFollower::new(out_path),
            err: LogFollower::new(err_path),
            out_lines: LineBuffer::new(),
            err_lines: LineBuffer::new(),
            prefix_out,
            prefix_err,
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
                let prefix = self.prefix_out.clone();
                self.out_lines
                    .push(&stdout, |line| emit(false, &prefix, line));
            }
        }
        if self.enabled_err {
            let stderr = self.err.read_new()?;
            if !stderr.is_empty() {
                any = true;
                let prefix = self.prefix_err.clone();
                self.err_lines
                    .push(&stderr, |line| emit(true, &prefix, line));
            }
        }
        Ok(any)
    }

    /// Emit any trailing partial line (called once when forwarding stops).
    pub fn flush(&mut self) {
        let prefix = self.prefix_out.clone();
        self.out_lines.flush(|line| emit(false, &prefix, line));
        let prefix = self.prefix_err.clone();
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
    fn sink_writes_markers_and_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let project = crate::paths::Project {
            root: dir.path().to_path_buf(),
            config_path: dir.path().join("agproc.toml"),
        };
        let mut sink = LogSink::open(&project, "svc", 32).unwrap();
        sink.marker("BUILDING").unwrap();
        sink.write_stdout(b"hello\n").unwrap();
        assert_eq!(sink.offsets().0, "===== BUILDING =====\nhello\n".len() as u64);

        // Crossing the cap rotates the file and starts a new one.
        sink.write_stdout(b"0123456789\n").unwrap();
        let rotated = dir.path().join(".agproc/logs/svc.stdout.log.1");
        assert!(rotated.exists());
        let current = std::fs::read_to_string(project.log_stdout("svc")).unwrap();
        assert_eq!(current, "0123456789\n");

        sink.write_stderr(b"boom\n").unwrap();
        let err = std::fs::read_to_string(project.log_stderr("svc")).unwrap();
        assert_eq!(err, "boom\n");
    }
}
