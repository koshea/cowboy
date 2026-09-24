//! `cowboy` — an opinionated local coding agent that runs inside a Docker
//! container while the host enforces security at the container and network
//! layer.

use anyhow::Result;
use clap::Parser;

use cowboy_cli::cli::{Cli, Command};
use cowboy_cli::cmd;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        // The command already printed a full report; adding `Error: …` to it would only
        // repeat (or empty out) what the user just read.
        Err(e) if e.downcast_ref::<cowboy_cli::AlreadyReported>().is_some() => {
            std::process::ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("Error: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    cowboy_cli::prompt::set_assume_yes(cli.yes);

    let start_flags = cli.start_flags();
    let resume = cli.resume_spec();
    match cli.command {
        // Bare `cowboy` or `cowboy "<task>"` -> session engine (Slice D).
        None => cmd::session::run(cli.task, start_flags, resume).await,
        Some(Command::Init(args)) => cmd::init::run(args),
        Some(Command::Doctor) => cmd::doctor::run().await,
        Some(Command::Sandbox(args)) => cmd::sandbox::run(args).await,
        Some(Command::Grant(args)) => cmd::grant::run(args),
        Some(Command::Shell) => cmd::run::shell().await,
        Some(Command::Run { command }) => cmd::run::run(command).await,
        Some(Command::Patch(args)) => cmd::patch::run(args).await,
        Some(Command::Proc(args)) => cmd::proc::run(args).await,
        Some(Command::Models(args)) => cmd::models::run(args).await,
        Some(Command::Harnesses) => cmd::harnesses::run(),
        Some(Command::Skill(args)) => cmd::skill::run(args),
        Some(Command::Agents(args)) => cmd::agents::run(args),
        Some(Command::Down(args)) => cmd::down::run(args).await,
        Some(Command::Web(args)) => match args.command {
            cowboy_cli::cli::WebCommand::On { bind, lan } => cmd::web::on(bind, lan).await,
            cowboy_cli::cli::WebCommand::Off => cmd::web::off().await,
            cowboy_cli::cli::WebCommand::Status => cmd::web::status().await,
        },
        Some(Command::Attach { target }) => cmd::attach::run(target).await,
        Some(Command::Sessions { all }) => cmd::sessions::run(all).await,
        Some(Command::Session(args)) => match args.command {
            cowboy_cli::cli::SessionCommand::List { all } => cmd::sessions::run(all).await,
            cowboy_cli::cli::SessionCommand::Cleanup { dry_run } => {
                cmd::sessions::cleanup(dry_run).await
            }
        },
        Some(Command::Worktree(args)) => match args.command {
            cowboy_cli::cli::WorktreeCommand::List => cmd::worktree::list().await,
            cowboy_cli::cli::WorktreeCommand::Create { name } => cmd::worktree::create(name).await,
            cowboy_cli::cli::WorktreeCommand::Diff { branch, session } => {
                cmd::worktree::diff(branch, session).await
            }
            cowboy_cli::cli::WorktreeCommand::Status { branch, session } => {
                cmd::worktree::status(branch, session).await
            }
        },
        Some(Command::Memory(args)) => cmd::memory::run(args),
        Some(Command::Secrets(args)) => cmd::secrets::run(args.command),
        Some(Command::Mcp(args)) => cmd::mcp::run(args.command).await,
        Some(Command::Artifact(args)) => cmd::artifact::run(args.command),
        Some(Command::Handoff { session }) => cmd::handoff::run(session),
        Some(Command::Decisions(args)) => cmd::decisions::run(args.command),
        Some(Command::Message { message, to, all }) => cmd::bus::send(message, to, all).await,
        Some(Command::Inbox { session, peek }) => cmd::bus::inbox(session, peek).await,
        Some(Command::Review { session, branch }) => cmd::review::run(session, branch),
        Some(Command::Ranch(args)) => cmd::ranch::run(args.command).await,
        Some(Command::Crew(args)) => cmd::crew::run(args.command).await,
        Some(Command::Logs) => cmd::logs::run().await,
        Some(Command::Replay { session_id, tui }) => cmd::logs::replay(session_id, tui).await,
        Some(Command::Completions { shell }) => {
            clap_complete::generate(
                shell,
                &mut <Cli as clap::CommandFactory>::command(),
                "cowboy",
                &mut std::io::stdout(),
            );
            Ok(())
        }
        Some(Command::XFileop) => cmd::fileop::run(),
        Some(Command::XForemanMcp { socket }) => {
            cowboy_cli::agent::harness::mcp::serve_stdio(socket).await
        }
        Some(Command::XSandboxShim) => cowboy_cli::sandbox::shim::run(),
        #[cfg(target_os = "linux")]
        Some(Command::XSandboxHolder) => cowboy_cli::sandbox::session::run_holder().await,
        // The session holder is a Linux namespace process; macOS has none.
        #[cfg(not(target_os = "linux"))]
        Some(Command::XSandboxHolder) => {
            anyhow::bail!("x-sandbox-holder exists only on Linux")
        }
        Some(Command::XSessionWorker(a)) => {
            cmd::worker::run(cmd::worker::WorkerArgs {
                root: a.root,
                task: a.task,
                sock: a.sock,
                id: a.id,
                register: a.register,
                resume: a.resume,
                ranch_id: a.ranch_id,
                workstream_id: a.workstream_id,
            })
            .await
        }
    }
}

fn init_tracing(verbose: bool) {
    use tracing_subscriber::{fmt, EnvFilter};
    // Quiet rmcp's per-request INFO chatter by default (the MCP client logs each
    // handshake/notification); override with COWBOY_LOG for full detail.
    let default = if verbose { "debug" } else { "info,rmcp=warn" };
    let filter = EnvFilter::try_from_env("COWBOY_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    // Logs go to stderr so they never pollute command/stdout capture.
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
