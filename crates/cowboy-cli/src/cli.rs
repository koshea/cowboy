//! Command-line surface for `cowboy`, defined with clap derive.

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "cowboy",
    version,
    about = "An opinionated local coding agent that runs wild inside a Docker corral.",
    long_about = "cowboy runs an AI coding agent inside a Docker container while the host \
                  enforces security at the container and network layer. The agent is never \
                  trusted to self-police."
)]
pub struct Cli {
    /// Optional one-shot task. With no subcommand, `cowboy 'fix the tests'`
    /// starts a session with the task prefilled.
    #[arg(value_name = "TASK")]
    pub task: Option<String>,

    /// Enable debug logging (or set COWBOY_LOG=...).
    #[arg(short, long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create initial project config files under `.cowboy/`.
    Init(InitArgs),

    /// Check Docker, Linux support, model config, network gateway, and Compose.
    Doctor,

    /// Open an interactive shell inside the agent container.
    Shell,

    /// Run a command inside the agent container.
    Run {
        /// The command and its arguments.
        #[arg(trailing_var_arg = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },

    /// Patch helper (wraps git inside the container).
    Patch(PatchArgs),

    /// Managed long-running process commands.
    Proc(ProcArgs),

    /// List session logs.
    Logs,

    /// Replay or inspect a previous session.
    Replay {
        #[arg(value_name = "SESSION_ID")]
        session_id: String,
    },
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Overwrite existing config files if present.
    #[arg(long)]
    pub force: bool,

    /// Also run `git init` if the project is not already a git repository.
    #[arg(long)]
    pub git: bool,
}

#[derive(Debug, Args)]
pub struct PatchArgs {
    #[command(subcommand)]
    pub command: PatchCommand,
}

#[derive(Debug, Subcommand)]
pub enum PatchCommand {
    /// Display the current git diff.
    Show,
    /// Save the current git diff to the session `diff.patch`.
    Save,
    /// Apply a patch read from stdin.
    Apply,
    /// Revert uncommitted changes (asks for confirmation).
    Revert,
    /// Validate that a patch from stdin applies cleanly.
    Check,
}

#[derive(Debug, Args)]
pub struct ProcArgs {
    #[command(subcommand)]
    pub command: ProcCommand,
}

#[derive(Debug, Subcommand)]
pub enum ProcCommand {
    /// List configured processes and their status.
    List,
    /// Start a process by name.
    Start { name: String },
    /// Stop a process by name.
    Stop { name: String },
    /// Restart a process by name.
    Restart { name: String },
    /// Stream logs for a process.
    Logs { name: String },
}
