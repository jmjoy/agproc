//! Orchestration lock: at most one `start`/`restart` per service at a time.
//!
//! The lock is held by the *CLI* for the duration of its call (build, run and
//! probe), not by the runner: the runner has to keep living after the CLI
//! exits, while a second `start` must be rejected. A lock left behind by a
//! killed CLI is released automatically by the kernel.

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

pub struct Lock {
    _file: File,
}

impl Lock {
    /// Take the lock without blocking. `Ok(None)` means another invocation
    /// currently holds it.
    pub fn try_acquire(path: &Path) -> Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {
                // Record our pid so the next caller can name the holder.
                let _ = file.set_len(0);
                let _ = file.seek(SeekFrom::Start(0));
                let _ = writeln!(file, "{}", std::process::id());
                Ok(Some(Self { _file: file }))
            }
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(err)) => Err(err)
                .with_context(|| format!("cannot lock {}", path.display())),
        }
    }

    /// PID recorded by the current holder, for a friendlier message.
    pub fn holder_pid(path: &Path) -> Option<u32> {
        let mut raw = String::new();
        File::open(path).ok()?.read_to_string(&mut raw).ok()?;
        raw.trim().parse().ok()
    }
}
