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
/// A single-service invocation is a pure pass-through (no prefix), while a
/// multi-service invocation labels every line so the merged view stays
/// readable. stdout and stderr share the label: the streams themselves stay
/// separate, so the reader does not need a textual marker for stderr.
#[derive(Clone, Debug)]
pub struct Prefix {
    /// `"backend  | "` in a multi-service invocation (the name padded to the
    /// longest one), `""` when a single service is a pure pass-through.
    pub label: String,
}

impl Prefix {
    pub fn plain() -> Self {
        Self {
            label: String::new(),
        }
    }

    /// `width` comes from [`prefix_width`].
    pub fn service(name: &str, width: Option<usize>) -> Self {
        match width {
            Some(width) => Self {
                label: format!("{name:<width$} | "),
            },
            None => Self::plain(),
        }
    }

    /// One of agproc's own `===== ... =====` lines.
    pub fn marker(&self, text: &str) {
        logstore::emit(false, &self.label, marker_text(text).as_bytes());
    }

    /// Free-form diagnostic line on our stderr.
    pub fn diagnostic(&self, text: &str) {
        logstore::emit(true, &self.label, text.as_bytes());
    }
}

/// The service-name column width of a multi-service invocation: every name is
/// padded to the longest one so the `|` columns line up. `None` means "one
/// service only", which is a pure pass-through with no prefix at all.
pub fn prefix_width<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<usize> {
    let mut count = 0;
    let mut width = 0;
    for name in names {
        count += 1;
        width = width.max(name.chars().count());
    }
    (count > 1).then_some(width)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_width_needs_at_least_two_services() {
        assert_eq!(prefix_width([]), None);
        assert_eq!(prefix_width(["backend"]), None);
        assert_eq!(prefix_width(["backend", "frontend"]), Some(8));
        // Order does not matter: it is the longest name that wins.
        assert_eq!(prefix_width(["frontend", "backend"]), Some(8));
        assert_eq!(prefix_width(["quiet", "other"]), Some(5));
    }

    #[test]
    fn service_prefixes_are_padded_to_one_width() {
        assert_eq!(Prefix::service("backend", Some(8)).label, "backend  | ");
        assert_eq!(Prefix::service("frontend", Some(8)).label, "frontend | ");
        // Equal-length names get no extra padding.
        assert_eq!(Prefix::service("quiet", Some(5)).label, "quiet | ");
        // A single service is a pure pass-through.
        assert_eq!(Prefix::service("backend", None).label, "");
        assert_eq!(Prefix::plain().label, "");
    }
}
