//! agproc — dev-time process manager for AI agents and humans.

mod cli;
mod cmd;
mod config;
mod exit;
mod lock;
mod logstore;
mod paths;
mod port;
mod probe;
mod proc;
mod procinfo;
mod runner;
mod state;
mod util;

fn main() {
    std::process::exit(cli::main());
}
