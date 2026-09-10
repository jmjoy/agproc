//! `agproc.toml` schema, loading and validation.
//!
//! Every key is kebab-case and unknown keys are rejected: a typo in a config an
//! agent just wrote must fail loudly instead of silently doing nothing.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::paths::Project;
use crate::util::fnv1a64;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Settings {
    /// Shell used for the string form of `build-cmd` / `run-cmd`.
    pub shell: String,
    /// Grace period between SIGTERM and SIGKILL on `stop`.
    pub stop_timeout_seconds: u64,
    /// Rotate a log file once it grows past this size (0 disables rotation).
    pub log_max_bytes: u64,
    /// Look for foreign listeners on the probe port before/after starting.
    pub port_check: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            shell: "sh".to_string(),
            stop_timeout_seconds: 10,
            log_max_bytes: 32 * 1024 * 1024,
            port_check: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default, rename = "service")]
    pub services: Vec<Service>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Service {
    pub name: String,
    /// Working directory, relative to the project root (default: the root).
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Omitted means "no build step"; the RUNNING phase starts directly.
    #[serde(default)]
    pub build_cmd: Option<Cmd>,
    /// 0 (the default) means no build timeout.
    #[serde(default)]
    pub build_timeout_seconds: Option<u64>,
    pub run_cmd: Cmd,
    /// Overrides `[settings] stop-timeout-seconds`.
    #[serde(default)]
    pub stop_timeout_seconds: Option<u64>,
    #[serde(default)]
    pub probe: Option<Probe>,
}

/// A command is either a shell string (`sh -c "..."`) or an argv array that is
/// executed directly without a shell.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Cmd {
    Shell(String),
    Argv(Vec<String>),
}

impl Cmd {
    pub fn argv(&self, shell: &str) -> Vec<String> {
        match self {
            Cmd::Shell(line) => vec![shell.to_string(), "-c".to_string(), line.clone()],
            Cmd::Argv(argv) => argv.clone(),
        }
    }

    pub fn display(&self) -> String {
        match self {
            Cmd::Shell(line) => line.clone(),
            Cmd::Argv(argv) => argv.join(" "),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Probe {
    pub http_get: Option<HttpGet>,
    pub tcp_connect: Option<TcpConnect>,
    pub initial_delay_seconds: u64,
    pub period_seconds: u64,
    pub timeout_seconds: u64,
    pub failure_threshold: u32,
}

impl Default for Probe {
    fn default() -> Self {
        Self {
            http_get: None,
            tcp_connect: None,
            initial_delay_seconds: 1,
            period_seconds: 1,
            timeout_seconds: 2,
            failure_threshold: 3,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct HttpGet {
    #[serde(default = "default_scheme")]
    pub scheme: String,
    #[serde(default = "default_host")]
    pub host: String,
    pub port: u16,
    #[serde(default = "default_path")]
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct TcpConnect {
    #[serde(default = "default_host")]
    pub host: String,
    pub port: u16,
}

fn default_scheme() -> String {
    "http".to_string()
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_path() -> String {
    "/".to_string()
}

/// The resolved probe target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeTarget {
    Http {
        scheme: String,
        host: String,
        port: u16,
        path: String,
    },
    Tcp {
        host: String,
        port: u16,
    },
    /// No probe configured: the service is ready once it is "still alive after the initial delay".
    None,
}

impl ProbeTarget {
    pub fn kind_str(&self) -> &'static str {
        match self {
            ProbeTarget::Http { .. } => "http-get",
            ProbeTarget::Tcp { .. } => "tcp-connect",
            ProbeTarget::None => "none",
        }
    }

    pub fn describe(&self) -> String {
        match self {
            ProbeTarget::Http {
                scheme,
                host,
                port,
                path,
            } => format!("{scheme}://{host}:{port}{path}"),
            ProbeTarget::Tcp { host, port } => format!("tcp {host}:{port}"),
            ProbeTarget::None => "none".to_string(),
        }
    }

    /// The local port to attribute, when the target is on this machine. Port
    /// ownership checks are skipped for non-loopback hosts.
    pub fn local_port(&self) -> Option<u16> {
        let host = match self {
            ProbeTarget::Http { host, .. } | ProbeTarget::Tcp { host, .. } => host,
            ProbeTarget::None => return None,
        };
        is_local_host(host).then(|| match self {
            ProbeTarget::Http { port, .. } | ProbeTarget::Tcp { port, .. } => *port,
            ProbeTarget::None => unreachable!(),
        })
    }
}

/// Hosts whose listeners we can attribute to local PIDs.
pub fn is_local_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

impl Probe {
    pub fn target(&self) -> ProbeTarget {
        if let Some(http) = &self.http_get {
            return ProbeTarget::Http {
                scheme: http.scheme.clone(),
                host: http.host.clone(),
                port: http.port,
                path: http.path.clone(),
            };
        }
        if let Some(tcp) = &self.tcp_connect {
            return ProbeTarget::Tcp {
                host: tcp.host.clone(),
                port: tcp.port,
            };
        }
        ProbeTarget::None
    }
}

impl Service {
    pub fn build_timeout(&self) -> u64 {
        self.build_timeout_seconds.unwrap_or(0)
    }

    pub fn stop_timeout(&self, settings: &Settings) -> u64 {
        self.stop_timeout_seconds
            .unwrap_or(settings.stop_timeout_seconds)
    }

    pub fn target(&self) -> ProbeTarget {
        self.probe
            .as_ref()
            .map(Probe::target)
            .unwrap_or(ProbeTarget::None)
    }

    /// Working directory, resolved against the project root and created if the
    /// rest of the command needs it (agproc never creates it itself).
    pub fn cwd_path(&self, project: &Project) -> PathBuf {
        match &self.cwd {
            Some(cwd) => project.root.join(cwd),
            None => project.root.clone(),
        }
    }
}

impl Config {
    pub fn service(&self, name: &str) -> Option<&Service> {
        self.services.iter().find(|s| s.name == name)
    }

    pub fn service_names(&self) -> Vec<&str> {
        self.services.iter().map(|s| s.name.as_str()).collect()
    }
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    /// Fingerprint of the raw file, stored in state so `ps` can tell that a
    /// running service was started from an older revision of the config.
    pub hash: String,
}

pub fn load(path: &Path) -> Result<LoadedConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read config {}", path.display()))?;
    let hash = format!("fnv1a64:{:016x}", fnv1a64(raw.as_bytes()));
    let config: Config = toml::from_str(&raw)
        .map_err(|err| anyhow::anyhow!("{}\n{}", path.display(), err))
        .context("invalid agproc.toml")?;
    validate(&config).with_context(|| format!("invalid config {}", path.display()))?;
    Ok(LoadedConfig { config, hash })
}

fn validate(config: &Config) -> Result<()> {
    if config.services.is_empty() {
        bail!("no [[service]] table declared");
    }
    for service in &config.services {
        validate_service(service)?;
    }
    let mut seen: Vec<&str> = Vec::new();
    for service in &config.services {
        if seen.contains(&service.name.as_str()) {
            bail!("duplicate service name \"{}\"", service.name);
        }
        seen.push(&service.name);
    }
    Ok(())
}

fn validate_service(service: &Service) -> Result<()> {
    let name = &service.name;
    if name.is_empty() {
        bail!("service name must not be empty");
    }
    let first = name.chars().next().unwrap_or('-');
    if !first.is_ascii_alphanumeric() {
        bail!("service \"{name}\": name must start with a letter or digit");
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.'))
    {
        bail!("service \"{name}\": name must not contain {bad:?} (allowed: letters, digits, '-', '_', '.')");
    }
    if let Cmd::Argv(argv) = &service.run_cmd
        && argv.is_empty()
    {
        bail!("service \"{name}\": run-cmd array must not be empty");
    }
    if let Some(Cmd::Argv(argv)) = &service.build_cmd
        && argv.is_empty()
    {
        bail!("service \"{name}\": build-cmd array must not be empty");
    }
    if let Some(probe) = &service.probe {
        validate_probe(name, probe)?;
    }
    Ok(())
}

fn validate_probe(name: &str, probe: &Probe) -> Result<()> {
    match (&probe.http_get, &probe.tcp_connect) {
        (Some(_), Some(_)) => bail!(
            "service \"{name}\": probe must set exactly one of http-get / tcp-connect, found both"
        ),
        (None, None) => bail!(
            "service \"{name}\": probe must set http-get or tcp-connect (remove the table to use liveness only)"
        ),
        _ => {}
    }
    if let Some(http) = &probe.http_get {
        if http.scheme != "http" {
            bail!(
                "service \"{name}\": probe scheme {:?} is not supported; only \"http\" is (local dev endpoints)",
                http.scheme
            );
        }
        if http.port == 0 {
            bail!("service \"{name}\": probe port must not be 0");
        }
        if http.host.trim().is_empty() {
            bail!("service \"{name}\": probe host must not be empty");
        }
    }
    if let Some(tcp) = &probe.tcp_connect {
        if tcp.port == 0 {
            bail!("service \"{name}\": probe port must not be 0");
        }
        if tcp.host.trim().is_empty() {
            bail!("service \"{name}\": probe host must not be empty");
        }
    }
    if probe.failure_threshold == 0 {
        bail!("service \"{name}\": probe failure-threshold must be at least 1");
    }
    if probe.period_seconds == 0 {
        bail!("service \"{name}\": probe period-seconds must be at least 1");
    }
    if probe.timeout_seconds == 0 {
        bail!("service \"{name}\": probe timeout-seconds must be at least 1");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<Config> {
        let config: Config = toml::from_str(raw).map_err(|e| anyhow::anyhow!("{e}"))?;
        validate(&config)?;
        Ok(config)
    }

    #[test]
    fn parses_documented_example() {
        let config = parse(
            r#"
[[service]]
name = "backend"
build-cmd = "cargo build"
run-cmd = "./target/debug/foo-backend"
probe = {
  http-get = { scheme = "http", host = "127.0.0.1", path = "/healthz", port = 3100 },
  initial-delay-seconds = 1,
  period-seconds = 1,
  timeout-seconds = 2,
  failure-threshold = 3,
}
"#,
        )
        .unwrap();
        let service = config.service("backend").unwrap();
        assert_eq!(service.build_cmd.as_ref().unwrap().display(), "cargo build");
        assert_eq!(
            service.target().describe(),
            "http://127.0.0.1:3100/healthz"
        );
        assert_eq!(service.target().local_port(), Some(3100));
        let probe = service.probe.as_ref().unwrap();
        assert_eq!(probe.failure_threshold, 3);
        assert_eq!(probe.period_seconds, 1);
    }

    #[test]
    fn probe_defaults_apply() {
        let config = parse(
            r#"
[[service]]
name = "api"
run-cmd = "sleep 1"
probe = { tcp-connect = { port = 5432 } }
"#,
        )
        .unwrap();
        let probe = config.services[0].probe.as_ref().unwrap();
        assert_eq!(probe.initial_delay_seconds, 1);
        assert_eq!(probe.period_seconds, 1);
        assert_eq!(probe.timeout_seconds, 2);
        assert_eq!(probe.failure_threshold, 3);
        assert_eq!(
            config.services[0].target().describe(),
            "tcp 127.0.0.1:5432"
        );
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = parse(
            r#"
[[service]]
name = "backend"
run_cmd = "true"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("run_cmd"), "{err}");
    }

    #[test]
    fn trailing_comma_is_rejected() {
        let err = parse(
            r#"
[[service]]
name = "backend"
run-cmd = "true"
probe = { tcp-connect = { port = 1, }, },
"#,
        )
        .unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn duplicate_names_rejected() {
        let err = parse(
            r#"
[[service]]
name = "a"
run-cmd = "true"

[[service]]
name = "a"
run-cmd = "true"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn missing_run_cmd_rejected() {
        let err = parse(
            r#"
[[service]]
name = "a"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("run-cmd"), "{err}");
    }

    #[test]
    fn probe_requires_exactly_one_kind() {
        let both = parse(
            r#"
[[service]]
name = "a"
run-cmd = "true"
probe = { http-get = { port = 1 }, tcp-connect = { port = 2 } }
"#,
        )
        .unwrap_err();
        assert!(both.to_string().contains("exactly one"), "{both}");

        let neither = parse(
            r#"
[[service]]
name = "a"
run-cmd = "true"
probe = { period-seconds = 1 }
"#,
        )
        .unwrap_err();
        assert!(neither.to_string().contains("http-get"), "{neither}");
    }

    #[test]
    fn https_scheme_is_rejected() {
        let err = parse(
            r#"
[[service]]
name = "a"
run-cmd = "true"
probe = { http-get = { scheme = "https", port = 443 } }
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");
    }

    #[test]
    fn argv_form_works_and_empty_argv_is_rejected() {
        let config = parse(
            r#"
[[service]]
name = "a"
run-cmd = ["cargo", "run", "--", "--flag"]
"#,
        )
        .unwrap();
        assert_eq!(
            config.services[0].run_cmd.argv("sh"),
            vec!["cargo", "run", "--", "--flag"]
        );

        let err = parse(
            r#"
[[service]]
name = "a"
run-cmd = []
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn settings_defaults_and_overrides() {
        let config = parse(
            r#"
[settings]
stop-timeout-seconds = 3

[[service]]
name = "a"
run-cmd = "true"
"#,
        )
        .unwrap();
        assert_eq!(config.settings.stop_timeout_seconds, 3);
        assert_eq!(config.settings.shell, "sh");
        assert!(config.settings.port_check);
        assert_eq!(config.settings.log_max_bytes, 32 * 1024 * 1024);
    }

    #[test]
    fn non_local_host_has_no_local_port() {
        let config = parse(
            r#"
[[service]]
name = "a"
run-cmd = "true"
probe = { http-get = { host = "example.com", port = 80 } }
"#,
        )
        .unwrap();
        assert_eq!(config.services[0].target().local_port(), None);
    }
}
