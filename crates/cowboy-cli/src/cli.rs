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

    /// On a same-worktree collision, attach to the active session instead of
    /// prompting.
    #[arg(long)]
    pub attach_if_active: bool,

    /// On a same-worktree collision, attach read-only (watch without driving).
    #[arg(long)]
    pub read_only: bool,

    /// On a same-worktree collision, create a new git worktree and run there.
    #[arg(long)]
    pub new_worktree: bool,

    /// Take over a *stale* lease on this worktree (never a live one).
    #[arg(long)]
    pub force_same_worktree: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    /// The same-worktree collision flags, bundled for the session engine.
    pub fn start_flags(&self) -> StartFlags {
        StartFlags {
            attach_if_active: self.attach_if_active,
            read_only: self.read_only,
            new_worktree: self.new_worktree,
            force: self.force_same_worktree,
        }
    }
}

/// How to resolve a same-worktree collision, set from CLI flags (otherwise the
/// user is prompted interactively).
#[derive(Debug, Clone, Copy, Default)]
pub struct StartFlags {
    pub attach_if_active: bool,
    pub read_only: bool,
    pub new_worktree: bool,
    pub force: bool,
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

    /// Configure model providers (home-owned) and models.
    Models(ModelsArgs),

    /// List or show agent skills (reusable instructions under .cowboy/skills/).
    Skill(SkillArgs),

    /// Stop and remove this project's agent + gateway containers and networks.
    Down(DownArgs),

    /// Attach the TUI to a running session (by id, or a worker socket path).
    Attach {
        #[arg(value_name = "SESSION")]
        target: String,
    },

    /// List sessions tracked by the daemon.
    Sessions,

    /// Session maintenance (reap stale records and their leases).
    Session(SessionCmdArgs),

    /// List or create git worktrees for parallel sessions.
    Worktree(WorktreeArgs),

    /// List session logs.
    Logs,

    /// Replay or inspect a previous session.
    Replay {
        #[arg(value_name = "SESSION_ID")]
        session_id: String,
    },

    /// Internal: in-container worker for the structured file tools (reads a JSON
    /// request on stdin). Not for direct use.
    #[command(name = "x-fileop", hide = true)]
    XFileop,

    /// Internal: headless session worker spawned by the daemon. Not for direct
    /// use.
    #[command(name = "x-session-worker", hide = true)]
    XSessionWorker(SessionWorkerArgs),
}

#[derive(Debug, Args)]
pub struct SessionWorkerArgs {
    /// Worktree root the session runs in.
    #[arg(long)]
    pub root: std::path::PathBuf,
    /// Optional initial task.
    #[arg(long)]
    pub task: Option<String>,
    /// Override the per-session socket path.
    #[arg(long)]
    pub sock: Option<std::path::PathBuf>,
    /// Daemon-assigned session id (used for the session dir + registry).
    #[arg(long)]
    pub id: Option<String>,
    /// Register with (and heartbeat to) the daemon.
    #[arg(long)]
    pub register: bool,
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

#[derive(Debug, Args)]
pub struct DownArgs {
    /// Remove ALL cowboy-managed containers and networks (every project).
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, Args)]
pub struct ModelsArgs {
    #[command(subcommand)]
    pub command: ModelsCommand,
}

#[derive(Debug, Subcommand)]
pub enum ModelsCommand {
    /// Interactively add a provider (endpoint + key, saved to your home dir)
    /// and a model that uses it.
    Setup,
    /// List configured providers and models, and the effective default.
    List,
    /// Set the default model. Writes to the project unless `--global`.
    Use {
        /// The model name to make default.
        name: String,
        /// Set the user-level (home) default instead of the project default.
        #[arg(short, long)]
        global: bool,
    },
    /// List models offered by the configured provider endpoints (chat models
    /// only unless `--all`), with recommended names and config status.
    Available {
        /// Include non-chat models (image/audio/embedding/etc).
        #[arg(long)]
        all: bool,
    },
    /// Register a model by its provider id, prefilled from shipped defaults.
    Add {
        /// The provider-side model id, e.g. `cerebras/zai-glm-4.7`.
        id: String,
        /// Friendly name (config key). Defaults to the recommended name.
        #[arg(long)]
        name: Option<String>,
        /// Provider to use (defaults to the only configured one).
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        temp: Option<f32>,
        #[arg(long)]
        context: Option<u32>,
        #[arg(long = "max-output")]
        max_output: Option<u32>,
        /// Reasoning effort: none|minimal|low|medium|high.
        #[arg(long)]
        reasoning: Option<String>,
        /// Make this the default model.
        #[arg(long)]
        default: bool,
    },
}

#[derive(Debug, Args)]
pub struct SessionCmdArgs {
    #[command(subcommand)]
    pub command: SessionCommand,
}

#[derive(Debug, Subcommand)]
pub enum SessionCommand {
    /// Reap stale (crashed/abandoned) session records and release their leases.
    /// Worktrees and branches are never touched.
    Cleanup {
        /// Show what would be reaped without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Args)]
pub struct WorktreeArgs {
    #[command(subcommand)]
    pub command: WorktreeCommand,
}

#[derive(Debug, Subcommand)]
pub enum WorktreeCommand {
    /// List git worktrees and any session occupying each.
    List,
    /// Create a `cowboy/<slug>` worktree off the current repo.
    Create {
        /// Task/branch hint used for the slug (e.g. "fix login").
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct SkillArgs {
    #[command(subcommand)]
    pub command: SkillCommand,
}

#[derive(Debug, Subcommand)]
pub enum SkillCommand {
    /// List available skills (name + description).
    List,
    /// Print a skill's instructions (to follow / pull into context).
    Show { name: String },
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
