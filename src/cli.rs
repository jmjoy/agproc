//! Command line surface and top-level error reporting.

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::time::Duration;

use crate::cmd::{init, logs, ps, skills, start, stop};
use crate::config;
use crate::exit;
use crate::paths::Project;
use crate::runner;

#[derive(Parser, Debug)]
#[command(
    name = "agproc",
    version,
    about = "Dev-time process manager for AI agents and humans",
    long_about = "agproc runs the services of a project (backend, frontend, ...) declared in agproc.toml:\n\
                  it builds them, runs them, waits for a probe and keeps state and logs under\n\
                  .agproc/, so repeated commands are safe for both humans and agents.",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Path to agproc.toml (default: search this directory and its parents)
    #[arg(short = 'C', long, global = true, env = "AGPROC_CONFIG", value_name = "PATH")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Build (when needed) and run services, then wait for the probe
    Start(StartArgs),
    /// Stop running services, then build and run them again
    Restart(StartArgs),
    /// Stop services that are building or running
    Stop(TargetArgs),
    /// Show what agproc is running
    Ps(PsArgs),
    /// Show service logs
    Logs(LogsArgs),
    /// Print the agent guide for this project
    Skills(SkillsArgs),
    /// Create an agproc.toml template in this directory
    Init(InitArgs),
    /// Internal per-service supervisor (spawned by `start`)
    #[command(name = "__runner", hide = true)]
    Runner(RunnerArgs),
}

#[derive(Args, Debug)]
pub struct StartArgs {
    /// Service names (default: every service)
    #[arg(value_name = "SERVICE")]
    pub services: Vec<String>,

    /// Stop waiting after this many seconds (the runner keeps working)
    #[arg(long, value_name = "SECONDS")]
    pub timeout_seconds: Option<u64>,
}

#[derive(Args, Debug)]
pub struct TargetArgs {
    /// Service names (default: every service)
    #[arg(value_name = "SERVICE")]
    pub services: Vec<String>,
}

#[derive(Args, Debug)]
pub struct PsArgs {
    /// Service names (default: every service)
    #[arg(value_name = "SERVICE")]
    pub services: Vec<String>,

    /// Machine readable output
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct LogsArgs {
    /// Service names (default: every service)
    #[arg(value_name = "SERVICE")]
    pub services: Vec<String>,

    /// Only the last N lines of the selected window
    #[arg(long, value_name = "N")]
    pub tail: Option<usize>,

    /// Keep printing new output; returns when the service stops
    #[arg(short = 'f', long)]
    pub follow: bool,

    /// Which stream to show
    #[arg(long, value_enum, default_value_t = StreamArg::Both)]
    pub stream: StreamArg,

    /// Show every session, not just the most recent one
    #[arg(long)]
    pub all: bool,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamArg {
    Both,
    Stdout,
    Stderr,
}

#[derive(Args, Debug)]
pub struct SkillsArgs {
    /// Machine readable output (guide plus structured project data)
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Overwrite an existing agproc.toml
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct RunnerArgs {
    /// Service to supervise
    #[arg(long)]
    pub service: String,

    /// Session generation (increments on every start/restart)
    #[arg(long)]
    pub generation: u64,
}

/// A failure that maps to a documented exit code.
#[derive(Debug)]
pub struct Failure {
    pub code: i32,
    pub title: &'static str,
    pub message: String,
}

impl Failure {
    pub fn new(code: i32, title: &'static str, message: impl std::fmt::Display) -> Self {
        Self {
            code,
            title,
            message: message.to_string(),
        }
    }

    pub fn generic(message: impl std::fmt::Display) -> Self {
        Self::new(exit::GENERIC, "ERROR", message)
    }

    pub fn config(message: impl std::fmt::Display) -> Self {
        Self::new(exit::CONFIG, "CONFIG ERROR", message)
    }

    /// Print in the agproc marker style so agents can recognise it.
    pub fn report(&self) {
        eprintln!("===== {} =====", self.title);
        eprintln!("{}", self.message);
    }
}

impl From<anyhow::Error> for Failure {
    fn from(err: anyhow::Error) -> Self {
        Failure::generic(format!("{err:#}"))
    }
}

pub fn main() -> i32 {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(code) => code,
        Err(failure) => {
            failure.report();
            failure.code
        }
    }
}

fn dispatch(cli: Cli) -> Result<i32, Failure> {
    let cwd = std::env::current_dir().map_err(Failure::generic)?;
    let explicit = cli.config.clone();

    match cli.command {
        Command::Init(args) => {
            let root = match &explicit {
                Some(path) if path.is_dir() => path.clone(),
                Some(path) => path
                    .parent()
                    .map(PathBuf::from)
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(cwd),
                None => cwd,
            };
            init::run(init::InitArgs {
                root,
                force: args.force,
            })
            .map_err(Failure::from)
        }

        Command::Runner(args) => {
            let config = explicit.clone().ok_or_else(|| {
                Failure::generic("__runner needs --config (it is an internal command)")
            })?;
            Ok(runner::run(runner::Args {
                config,
                service: args.service,
                generation: args.generation,
            }))
        }

        Command::Skills(args) => {
            // Works outside a project too, so the agent can learn how to set one up.
            let project = Project::discover_optional(explicit.as_deref())
                .map_err(|err| Failure::config(format!("{err:#}")))?;
            skills::run(project.as_ref(), skills::Request { json: args.json })
        }

        Command::Start(args) => {
            let (project, loaded) = open(explicit.as_deref())?;
            start::run(
                &project,
                &loaded.config,
                start::Request {
                    services: args.services,
                    restart: false,
                    timeout: args.timeout_seconds.map(Duration::from_secs),
                },
            )
        }

        Command::Restart(args) => {
            let (project, loaded) = open(explicit.as_deref())?;
            start::run(
                &project,
                &loaded.config,
                start::Request {
                    services: args.services,
                    restart: true,
                    timeout: args.timeout_seconds.map(Duration::from_secs),
                },
            )
        }

        Command::Stop(args) => {
            let (project, loaded) = open(explicit.as_deref())?;
            let services = crate::cmd::select_services(&loaded.config, &args.services)
                .map_err(Failure::config)?;
            project.ensure_layout().map_err(Failure::from)?;
            stop::run(&project, &loaded.config, services)
        }

        Command::Ps(args) => {
            let (project, loaded) = open(explicit.as_deref())?;
            ps::run(
                &project,
                &loaded.config,
                ps::Request {
                    services: args.services,
                    json: args.json,
                },
            )
        }

        Command::Logs(args) => {
            let (project, loaded) = open(explicit.as_deref())?;
            logs::run(
                &project,
                &loaded.config,
                logs::Request {
                    services: args.services,
                    tail: args.tail,
                    follow: args.follow,
                    stream: match args.stream {
                        StreamArg::Both => logs::Stream::Both,
                        StreamArg::Stdout => logs::Stream::Stdout,
                        StreamArg::Stderr => logs::Stream::Stderr,
                    },
                    all: args.all,
                },
            )
        }
    }
}

fn open(explicit: Option<&std::path::Path>) -> Result<(Project, config::LoadedConfig), Failure> {
    let project =
        Project::discover(explicit).map_err(|err| Failure::config(format!("{err:#}")))?;
    let loaded = config::load(&project.config_path)
        .map_err(|err| Failure::config(format!("{err:#}")))?;
    Ok((project, loaded))
}
