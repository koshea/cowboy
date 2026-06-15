//! `cowboy` — an opinionated local coding agent that runs inside a Docker
//! container while the host enforces security at the container and network
//! layer.

use anyhow::Result;
use clap::Parser;

use cowboy_cli::cli::{Cli, Command};
use cowboy_cli::cmd;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let start_flags = cli.start_flags();
    let resume = cli.resume_spec();
    match cli.command {
        // Bare `cowboy` or `cowboy "<task>"` -> session engine (Slice D).
        None => cmd::session::run(cli.task, start_flags, resume).await,
        Some(Command::Init(args)) => cmd::init::run(args),
        Some(Command::Doctor) => cmd::doctor::run().await,
        Some(Command::Shell) => cmd::run::shell().await,
        Some(Command::Run { command }) => cmd::run::run(command).await,
        Some(Command::Patch(args)) => cmd::patch::run(args).await,
        Some(Command::Proc(args)) => cmd::proc::run(args).await,
        Some(Command::Models(args)) => cmd::models::run(args).await,
        Some(Command::Skill(args)) => cmd::skill::run(args),
        Some(Command::Down(args)) => cmd::down::run(args).await,
        Some(Command::Attach { target }) => cmd::attach::run(target).await,
        Some(Command::Sessions) => cmd::sessions::run().await,
        Some(Command::Session(args)) => match args.command {
            cowboy_cli::cli::SessionCommand::Cleanup { dry_run } => {
                cmd::sessions::cleanup(dry_run).await
            }
        },
        Some(Command::Worktree(args)) => match args.command {
            cowboy_cli::cli::WorktreeCommand::List => cmd::worktree::list().await,
            cowboy_cli::cli::WorktreeCommand::Create { name } => cmd::worktree::create(name).await,
        },
        Some(Command::Memory(args)) => cmd::memory::run(args),
        Some(Command::Secrets(args)) => cmd::secrets::run(args.command),
        Some(Command::Artifact(args)) => cmd::artifact::run(args.command),
        Some(Command::Handoff { session }) => cmd::handoff::run(session),
        Some(Command::Decisions(args)) => cmd::decisions::run(args.command),
        Some(Command::Message { message, to, all }) => cmd::bus::send(message, to, all).await,
        Some(Command::Inbox { session }) => cmd::bus::inbox(session).await,
        Some(Command::Logs) => cmd::logs::run().await,
        Some(Command::Replay { session_id }) => cmd::logs::replay(session_id).await,
        Some(Command::XFileop) => cmd::fileop::run(),
        Some(Command::XSessionWorker(a)) => {
            cmd::worker::run(cmd::worker::WorkerArgs {
                root: a.root,
                task: a.task,
                sock: a.sock,
                id: a.id,
                register: a.register,
                resume: a.resume,
            })
            .await
        }
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
