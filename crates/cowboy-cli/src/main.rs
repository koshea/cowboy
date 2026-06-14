//! `cowboy` — an opinionated local coding agent that runs inside a Docker
//! container while the host enforces security at the container and network
//! layer.

mod agent;
mod cli;
mod cmd;
mod net;
mod session;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        // Bare `cowboy` or `cowboy "<task>"` -> session engine (Slice D).
        None => cmd::session::run(cli.task, /* one_shot */ false).await,
        Some(Command::Init(args)) => cmd::init::run(args),
        Some(Command::Doctor) => cmd::doctor::run().await,
        Some(Command::Shell) => cmd::run::shell().await,
        Some(Command::Run { command }) => cmd::run::run(command).await,
        Some(Command::Patch(args)) => cmd::patch::run(args).await,
        Some(Command::Proc(args)) => cmd::proc::run(args).await,
        Some(Command::Skill(args)) => cmd::skill::run(args),
        Some(Command::Logs) => cmd::logs::run().await,
        Some(Command::Replay { session_id }) => cmd::logs::replay(session_id).await,
    }
}

fn init_tracing(verbose: bool) {
    use tracing_subscriber::{fmt, EnvFilter};
    let default = if verbose { "debug" } else { "info" };
    let filter = EnvFilter::try_from_env("COWBOY_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    // Logs go to stderr so they never pollute command/stdout capture.
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
