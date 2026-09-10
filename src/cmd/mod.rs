//! Subcommand implementations and the output conventions they share.

pub mod init;
pub mod logs;
pub mod ps;
pub mod skills;
pub mod start;
pub mod stop;

use crate::config::{Config, Service};
use crate::logstore;

/// Log routing for the current invocation.
///
/// A single-service invocation is a pure pass-through (no prefixes), while a
/// multi-service invocation labels every line so the merged view stays
/// readable.
#[derive(Clone, Debug)]
pub struct Prefix {
    pub out: String,
    pub err: String,
}

impl Prefix {
    pub fn plain() -> Self {
        Self {
            out: String::new(),
            err: String::new(),
        }
    }

    pub fn service(name: &str, multi: bool) -> Self {
        if multi {
            Self {
                out: format!("{name} | "),
                err: format!("{name} stderr | "),
            }
        } else {
            Self::plain()
        }
    }

    /// One of agproc's own `===== ... =====` lines.
    pub fn marker(&self, text: &str) {
        logstore::emit(false, &self.out, marker_text(text).as_bytes());
    }

    /// Free-form diagnostic line on our stderr.
    pub fn diagnostic(&self, text: &str) {
        logstore::emit(true, &self.err, text.as_bytes());
    }
}

pub fn marker_text(text: &str) -> String {
    format!("===== {text} =====")
}

/// Resolve the services named on the command line (empty means "all").
pub fn select_services<'a>(
    config: &'a Config,
    requested: &[String],
) -> Result<Vec<&'a Service>, String> {
    if requested.is_empty() {
        return Ok(config.services.iter().collect());
    }
    let mut selected = Vec::new();
    for name in requested {
        match config.service(name) {
            Some(service) => {
                if !selected.iter().any(|s: &&Service| s.name == service.name) {
                    selected.push(service);
                }
            }
            None => {
                return Err(format!(
                    "unknown service \"{name}\"; known services: {}",
                    config.service_names().join(", ")
                ));
            }
        }
    }
    Ok(selected)
}
