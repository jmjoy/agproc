//! Project root discovery and the `.agproc/` runtime layout.
//!
//! `agproc.toml` is looked up from the current directory upwards, like `git`
//! and `cargo` do, so every command works from any subdirectory of the
//! project. All runtime files live in `<root>/.agproc/`.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "agproc.toml";
pub const AGPROC_DIR: &str = ".agproc";
/// Bumped whenever the on-disk meaning changes; a mismatch makes agproc treat
/// the directory as having no usable history instead of misreading it.
pub const LAYOUT_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub config_path: PathBuf,
}

impl Project {
    /// Discover the project, failing when no `agproc.toml` can be found.
    pub fn discover(explicit: Option<&Path>) -> Result<Self> {
        match Self::discover_optional(explicit)? {
            Some(project) => Ok(project),
            None => match explicit {
                Some(path) => bail!(
                    "config file not found: {}\n(use -C/--config or AGPROC_CONFIG to point at agproc.toml)",
                    path.display()
                ),
                None => {
                    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                    bail!(
                        "no {CONFIG_FILE} found in {} or any parent directory\n(run `agproc init` to create one)",
                        cwd.display()
                    )
                }
            },
        }
    }

    /// Discover the project, returning `None` when there is none (used by
    /// `agproc skills`, which must also work outside a project).
    pub fn discover_optional(explicit: Option<&Path>) -> Result<Option<Self>> {
        if let Some(path) = explicit {
            let path = if path.is_dir() {
                path.join(CONFIG_FILE)
            } else {
                path.to_path_buf()
            };
            let path = absolute(&path)?;
            if !path.is_file() {
                return Ok(None);
            }
            let root = path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            return Ok(Some(Self {
                root,
                config_path: path,
            }));
        }

        let cwd = std::env::current_dir().context("cannot determine current directory")?;
        let mut dir: &Path = &cwd;
        loop {
            let candidate = dir.join(CONFIG_FILE);
            if candidate.is_file() {
                return Ok(Some(Self {
                    root: dir.to_path_buf(),
                    config_path: candidate,
                }));
            }
            match dir.parent() {
                Some(parent) => dir = parent,
                None => return Ok(None),
            }
        }
    }

    pub fn agproc_dir(&self) -> PathBuf {
        self.root.join(AGPROC_DIR)
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.agproc_dir().join("logs")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.agproc_dir().join("state")
    }

    pub fn lock_dir(&self) -> PathBuf {
        self.agproc_dir().join("lock")
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.agproc_dir().join("tmp")
    }

    pub fn log_stdout(&self, service: &str) -> PathBuf {
        self.logs_dir().join(format!("{service}.stdout.log"))
    }

    pub fn log_stderr(&self, service: &str) -> PathBuf {
        self.logs_dir().join(format!("{service}.stderr.log"))
    }

    pub fn state_path(&self, service: &str) -> PathBuf {
        self.state_dir().join(format!("{service}.json"))
    }

    pub fn stop_marker(&self, service: &str) -> PathBuf {
        self.state_dir().join(format!("{service}.stop-requested"))
    }

    pub fn lock_path(&self, service: &str) -> PathBuf {
        self.lock_dir().join(format!("{service}.lock"))
    }

    pub fn version_path(&self) -> PathBuf {
        self.agproc_dir().join("version")
    }

    /// Create the runtime directories. Safe to call from every command.
    pub fn ensure_layout(&self) -> Result<()> {
        for dir in [
            self.logs_dir(),
            self.state_dir(),
            self.lock_dir(),
            self.tmp_dir(),
        ] {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let version = self.version_path();
        if !version.exists() {
            let body = format!(
                "layout = {LAYOUT_VERSION}\nagproc = {}\n",
                env!("CARGO_PKG_VERSION")
            );
            crate::util::write_atomic(&version, &self.tmp_dir(), body.as_bytes())?;
        }
        Ok(())
    }

    /// Warning to print when the directory was written by an incompatible
    /// agproc; `None` when everything matches.
    pub fn layout_warning(&self) -> Option<String> {
        let raw = std::fs::read_to_string(self.version_path()).ok()?;
        let layout = raw.lines().find_map(|line| {
            line.strip_prefix("layout = ")
                .and_then(|v| v.trim().parse::<u32>().ok())
        })?;
        if layout == LAYOUT_VERSION {
            return None;
        }
        Some(format!(
            "{} was written by an incompatible agproc (layout {layout}, this build expects {LAYOUT_VERSION}); \
             history in it is ignored",
            self.agproc_dir().display()
        ))
    }
}

/// Make `path` absolute and symlink-free without requiring it to exist.
pub fn absolute(path: &Path) -> Result<PathBuf> {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Ok(canonical);
    }
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("cannot determine current directory")?
            .join(path)
    };
    Ok(joined)
}
