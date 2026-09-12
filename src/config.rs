//! `agproc.toml` schema, loading and validation.
//!
//! Every key is kebab-case and unknown keys are rejected: a typo in a config an
//! agent just wrote must fail loudly instead of silently doing nothing.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::{self, SeqAccess, Visitor};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::paths::Project;
use crate::util::fnv1a64;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Settings {
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
    /// Optional dotenv file (relative to the project root) whose `KEY=VALUE`
    /// pairs join the environment of both `build-cmd` and `run-cmd`; `env` wins
    /// over it.
    #[serde(default)]
    pub env_file: Option<String>,
    /// Omitted means "no build step"; the RUNNING phase starts directly.
    #[serde(default)]
    pub build_cmd: Option<Cmd>,
    /// 0 (the default) means no build timeout.
    #[serde(default)]
    pub build_timeout_seconds: Option<u64>,
    /// argv array, executed directly (no shell).
    pub run_cmd: Cmd,
    /// Overrides `[settings] stop-timeout-seconds`.
    #[serde(default)]
    pub stop_timeout_seconds: Option<u64>,
    #[serde(default)]
    pub probe: Option<Probe>,
}

/// A command is an argv array — program first, arguments after — executed
/// directly without a shell. Shell syntax (pipes, `&&`, redirections, globs,
/// variables) needs an explicit shell: `["sh", "-c", "..."]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd(Vec<String>);

impl Cmd {
    pub fn argv(&self) -> &[String] {
        &self.0
    }

    /// The command as a TOML array literal, so what an agent reads in
    /// `agproc skills` is exactly what belongs in the config: arguments with
    /// spaces or shell metacharacters stay unambiguous.
    pub fn display(&self) -> String {
        let mut out = String::from("[");
        for (index, arg) in self.0.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push('"');
            for ch in arg.chars() {
                match ch {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
                    c => out.push(c),
                }
            }
            out.push('"');
        }
        out.push(']');
        out
    }
}

impl<'de> Deserialize<'de> for Cmd {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct CmdVisitor;

        impl<'de> Visitor<'de> for CmdVisitor {
            type Value = Cmd;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an array of strings, e.g. [\"cargo\", \"build\"]")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Cmd, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut argv = Vec::new();
                while let Some(arg) = seq.next_element::<String>()? {
                    argv.push(arg);
                }
                Ok(Cmd(argv))
            }

            fn visit_str<E>(self, _value: &str) -> Result<Cmd, E>
            where
                E: de::Error,
            {
                // The string form used to run `<shell> -c "<value>"`; it is gone.
                Err(E::custom(
                    "commands are argv arrays, e.g. build-cmd = [\"cargo\", \"build\"]; \
                     write [\"sh\", \"-c\", \"<command line>\"] if you need shell syntax \
                     (the string form that ran `<shell> -c` was removed)",
                ))
            }
        }

        deserializer.deserialize_any(CmdVisitor)
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

    /// The env-file resolved against the project root (an absolute path stays as
    /// it is). The file is read by the runner at session start, so editing it
    /// takes effect on the next `agproc restart`.
    pub fn env_file_path(&self, project: &Project) -> Option<PathBuf> {
        self.env_file.as_ref().map(|file| project.root.join(file))
    }

    /// The environment the service's children actually get: the env-file first,
    /// then the `env` table on top of it. Everything else is inherited from the
    /// runner through `Command`, which is why only these two sources are listed.
    pub fn spawn_env(&self, project: &Project) -> Result<BTreeMap<String, String>> {
        let mut env = BTreeMap::new();
        if let Some(path) = self.env_file_path(project) {
            env = crate::envfile::load(&path)
                .with_context(|| format!("service \"{}\"", self.name))?;
        }
        for (key, value) in &self.env {
            env.insert(key.clone(), value.clone());
        }
        Ok(env)
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
    /// Fingerprint of the raw file plus every `env-file` it references, stored in
    /// state so `ps` can tell that a running service was started from an older
    /// revision of the configuration.
    pub hash: String,
}

pub fn load(path: &Path) -> Result<LoadedConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read config {}", path.display()))?;
    let config: Config = toml::from_str(&raw)
        .map_err(|err| anyhow::anyhow!("{}\n{}", path.display(), err))
        .context("invalid agproc.toml")?;
    validate(&config).with_context(|| format!("invalid config {}", path.display()))?;
    let hash = fingerprint(path, &raw, &config);
    Ok(LoadedConfig { config, hash })
}

/// The configuration revision: the raw `agproc.toml` plus, for every service that
/// declares one, its `env-file` (a null separator, the service name, the resolved
/// path and the bytes; unreadable files contribute a fixed marker).
///
/// Editing a `.env` therefore shows up exactly like editing `agproc.toml`, which
/// is what tells `ps` and `start` to say "config changed since start". Reading the
/// file must never fail here: `ps`, `logs` and `stop` have to keep working while
/// the runner is the one that refuses to start.
fn fingerprint(config_path: &Path, raw: &str, config: &Config) -> String {
    let root = config_path.parent().unwrap_or_else(|| Path::new("."));
    let mut buffer = Vec::from(raw.as_bytes());
    for service in &config.services {
        let Some(file) = &service.env_file else {
            continue;
        };
        let path = root.join(file);
        buffer.push(0);
        buffer.extend_from_slice(service.name.as_bytes());
        buffer.push(0);
        buffer.extend_from_slice(path.as_os_str().as_encoded_bytes());
        buffer.push(0);
        match std::fs::read(&path) {
            Ok(bytes) => buffer.extend_from_slice(&bytes),
            Err(_) => buffer.extend_from_slice(b"<unreadable>"),
        }
    }
    format!("fnv1a64:{:016x}", fnv1a64(&buffer))
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
    validate_cmd(name, "run-cmd", &service.run_cmd)?;
    if let Some(build_cmd) = &service.build_cmd {
        validate_cmd(name, "build-cmd", build_cmd)?;
    }
    if let Some(file) = &service.env_file
        && file.trim().is_empty()
    {
        bail!(
            "service \"{name}\": env-file must not be empty (remove the key when the service needs no env file)"
        );
    }
    if let Some(probe) = &service.probe {
        validate_probe(name, probe)?;
    }
    Ok(())
}

fn validate_cmd(name: &str, key: &str, cmd: &Cmd) -> Result<()> {
    match cmd.argv().first() {
        None => bail!("service \"{name}\": {key} must not be empty"),
        Some(program) if program.trim().is_empty() => {
            bail!("service \"{name}\": {key} must start with a program name")
        }
        Some(_) => Ok(()),
    }
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
build-cmd = ["cargo", "build"]
run-cmd = ["./target/debug/foo-backend"]
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
        assert_eq!(
            service.build_cmd.as_ref().unwrap().display(),
            r#"["cargo", "build"]"#
        );
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
run-cmd = ["sleep", "1"]
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
run-cmd = ["true"]
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
run-cmd = ["true"]

[[service]]
name = "a"
run-cmd = ["true"]
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
run-cmd = ["true"]
probe = { http-get = { port = 1 }, tcp-connect = { port = 2 } }
"#,
        )
        .unwrap_err();
        assert!(both.to_string().contains("exactly one"), "{both}");

        let neither = parse(
            r#"
[[service]]
name = "a"
run-cmd = ["true"]
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
run-cmd = ["true"]
probe = { http-get = { scheme = "https", port = 443 } }
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");
    }

    #[test]
    fn argv_is_the_only_form_and_empty_argv_is_rejected() {
        let config = parse(
            r#"
[[service]]
name = "a"
run-cmd = ["cargo", "run", "--", "--flag"]
"#,
        )
        .unwrap();
        assert_eq!(
            config.services[0].run_cmd.argv(),
            ["cargo", "run", "--", "--flag"]
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

        let blank = parse(
            r#"
[[service]]
name = "a"
run-cmd = ["", "arg"]
"#,
        )
        .unwrap_err();
        assert!(blank.to_string().contains("program name"), "{blank}");
    }

    #[test]
    fn string_form_is_rejected_with_a_hint() {
        let err = parse(
            r#"
[[service]]
name = "a"
run-cmd = "cargo run"
"#,
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("argv array"), "{text}");
        assert!(text.contains("[\"sh\", \"-c\""), "{text}");

        let build = parse(
            r#"
[[service]]
name = "a"
build-cmd = "cargo build"
run-cmd = ["true"]
"#,
        )
        .unwrap_err();
        assert!(build.to_string().contains("argv array"), "{build}");
    }

    #[test]
    fn settings_defaults_and_overrides() {
        let config = parse(
            r#"
[settings]
stop-timeout-seconds = 3

[[service]]
name = "a"
run-cmd = ["true"]
"#,
        )
        .unwrap();
        assert_eq!(config.settings.stop_timeout_seconds, 3);
        assert!(config.settings.port_check);
        assert_eq!(config.settings.log_max_bytes, 32 * 1024 * 1024);

        // The shell setting only existed for the removed string form.
        let err = parse(
            r#"
[settings]
shell = "sh"

[[service]]
name = "a"
run-cmd = ["true"]
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("shell"), "{err}");
    }

    #[test]
    fn display_renders_a_toml_array_literal() {
        let config = parse(
            r#"
[[service]]
name = "a"
run-cmd = ["sh", "-c", "echo \"hi\" > out; sleep 1"]
"#,
        )
        .unwrap();
        assert_eq!(
            config.services[0].run_cmd.display(),
            r#"["sh", "-c", "echo \"hi\" > out; sleep 1"]"#
        );
    }

    #[test]
    fn non_local_host_has_no_local_port() {
        let config = parse(
            r#"
[[service]]
name = "a"
run-cmd = ["true"]
probe = { http-get = { host = "example.com", port = 80 } }
"#,
        )
        .unwrap();
        assert_eq!(config.services[0].target().local_port(), None);
    }

    // ------------------------------------------------------------- env-file

    /// A project on disk, so `env-file` can actually be read.
    fn project_with(dir: &Path, config: &str) -> Project {
        let path = dir.join("agproc.toml");
        std::fs::write(&path, config).unwrap();
        Project {
            root: dir.to_path_buf(),
            config_path: path,
        }
    }

    fn write(dir: &Path, relative: &str, body: &str) {
        let path = dir.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn env_file_name_is_kebab_case() {
        let err = parse(
            r#"
[[service]]
name = "a"
run-cmd = ["true"]
env_file = ".env"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("env_file"), "{err}");
    }

    #[test]
    fn env_file_must_not_be_empty() {
        let err = parse(
            r#"
[[service]]
name = "a"
run-cmd = ["true"]
env-file = "  "
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("env-file"), "{err}");
    }

    #[test]
    fn env_file_path_is_relative_to_the_project_root() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with(
            dir.path(),
            r#"
[[service]]
name = "a"
run-cmd = ["true"]
env-file = "config/dev.env"
"#,
        );
        let loaded = load(&project.config_path).unwrap();
        let service = loaded.config.service("a").unwrap();
        assert_eq!(
            service.env_file_path(&project).unwrap(),
            dir.path().join("config/dev.env")
        );

        // An absolute path is used as it is.
        let absolute = dir.path().join("absolute.env");
        let project = project_with(
            dir.path(),
            &format!(
                "[[service]]\nname = \"a\"\nrun-cmd = [\"true\"]\nenv-file = \"{}\"\n",
                absolute.display()
            ),
        );
        let loaded = load(&project.config_path).unwrap();
        assert_eq!(
            loaded.config.service("a").unwrap().env_file_path(&project),
            Some(absolute)
        );
    }

    #[test]
    fn spawn_env_merges_the_file_under_the_env_table() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ".env",
            "# from the file\nFROM_FILE=file\nWHO=file\nPORT=\"5432\"\n",
        );
        let project = project_with(
            dir.path(),
            r#"
[[service]]
name = "backend"
run-cmd = ["true"]
env-file = ".env"
env = { WHO = "inline", ONLY_INLINE = "yes" }
"#,
        );
        let loaded = load(&project.config_path).unwrap();
        let env = loaded
            .config
            .service("backend")
            .unwrap()
            .spawn_env(&project)
            .unwrap();
        assert_eq!(env["FROM_FILE"], "file");
        assert_eq!(env["PORT"], "5432");
        // The explicit table wins over the file.
        assert_eq!(env["WHO"], "inline");
        assert_eq!(env["ONLY_INLINE"], "yes");
        assert_eq!(env.len(), 4);
    }

    #[test]
    fn spawn_env_without_an_env_file_is_just_the_env_table() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with(
            dir.path(),
            r#"
[[service]]
name = "a"
run-cmd = ["true"]
env = { A = "1" }
"#,
        );
        let loaded = load(&project.config_path).unwrap();
        let env = loaded
            .config
            .service("a")
            .unwrap()
            .spawn_env(&project)
            .unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env["A"], "1");
    }

    #[test]
    fn a_missing_env_file_names_the_resolved_path() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with(
            dir.path(),
            r#"
[[service]]
name = "backend"
run-cmd = ["true"]
env-file = "config/dev.env"
"#,
        );
        let loaded = load(&project.config_path).unwrap();
        // Loading the config itself must keep working: `ps` and `stop` still need it.
        let err = loaded
            .config
            .service("backend")
            .unwrap()
            .spawn_env(&project)
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("service \"backend\""), "{text}");
        assert!(text.contains("cannot read env-file"), "{text}");
        assert!(
            text.contains(&dir.path().join("config/dev.env").display().to_string()),
            "{text}"
        );
    }

    #[test]
    fn the_fingerprint_tracks_env_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with(
            dir.path(),
            r#"
[[service]]
name = "a"
run-cmd = ["true"]
env-file = ".env"
"#,
        );
        write(dir.path(), ".env", "A=1\n");
        let first = load(&project.config_path).unwrap().hash;

        // Same bytes: same revision.
        assert_eq!(load(&project.config_path).unwrap().hash, first);

        write(dir.path(), ".env", "A=2\n");
        let edited = load(&project.config_path).unwrap().hash;
        assert_ne!(edited, first, "editing .env must change the fingerprint");

        std::fs::remove_file(dir.path().join(".env")).unwrap();
        let deleted = load(&project.config_path).unwrap().hash;
        assert_ne!(deleted, edited, "deleting .env must change the fingerprint");

        // A service without env-file keeps the plain config fingerprint.
        let plain = project_with(
            dir.path(),
            "[[service]]\nname = \"a\"\nrun-cmd = [\"true\"]\n",
        );
        let first_plain = load(&plain.config_path).unwrap().hash;
        assert_eq!(load(&plain.config_path).unwrap().hash, first_plain);
        assert_ne!(first_plain, first);
    }
}
