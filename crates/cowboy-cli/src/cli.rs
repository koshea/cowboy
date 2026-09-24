//! Command-line surface for `cowboy`, defined with clap derive.
//!
//! Two conventions worth knowing before adding to this file.
//!
//! **Enumerated values are `ValueEnum`s, not `String`s.** `--transport`, `--kind` and
//! `--reasoning` each used to be a string parsed in the command body, so the only way to
//! learn the accepted values was to guess wrong and read the error — `--help` listed
//! nothing, and `cowboy completions` could not offer them. The variants live here rather
//! than in `cowboy-core` because core is deliberately clap-free; each one converts into
//! its core counterpart.
//!
//! **Aliases absorb the plural/singular coin flips**, so `cowboy skills` and
//! `cowboy skill` both work. They are hidden from the command list (`visible_alias` would
//! double its length) but are listed in each command's own `--help`.
//!
//! **Examples live in `after_help`, and they are tested.** Every line in one of these
//! blocks that begins with `cowboy ` is parsed by the real command tree in
//! `tests/cli_docs.rs`, so an example cannot survive the flag it demonstrates being
//! renamed. Write them as complete, runnable commands for that reason — a fragment or a
//! `…` placeholder will fail the check.

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Examples for the root command.
///
/// Answers the question a bare `--help` did not: of thirty-odd subcommands, which three
/// do you need on day one?
const ROOT_EXAMPLES: &str = "\
Getting started:
  cowboy init                     # set up .cowboy/ in this repo
  cowboy models setup             # configure a provider + model (once per machine)
  cowboy doctor                   # check the host can sandbox and the config is sane

Everyday use:
  cowboy                          # open the TUI and pick a task
  cowboy \"fix the failing tests\"  # start with the task prefilled
  cowboy --continue               # resume the most recent session in this worktree
  cowboy sessions                 # list sessions, then: cowboy attach <id>
  cowboy down                     # end this project's sessions

Type /help inside the TUI for keys and slash commands.";

#[derive(Debug, Parser)]
#[command(
    name = "cowboy",
    version,
    about = "An opinionated local coding agent that runs wild inside a corral you own.",
    long_about = "cowboy runs an AI coding agent in a sandbox built from your own machine \
                  — namespaces, Landlock, seccomp and a sole-egress gateway — while the \
                  host enforces the boundary. The agent is never trusted to self-police.",
    after_help = ROOT_EXAMPLES
)]
pub struct Cli {
    /// Optional one-shot task. With no subcommand, `cowboy 'fix the tests'`
    /// starts a session with the task prefilled.
    #[arg(value_name = "TASK")]
    pub task: Option<String>,

    /// Enable debug logging (or set COWBOY_LOG=...).
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Answer every confirmation with yes (or set COWBOY_ASSUME_YES=1).
    #[arg(short = 'y', long, global = true)]
    pub yes: bool,

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

    /// Continue the most recent session in this worktree, keeping its history.
    #[arg(long = "continue")]
    pub continue_latest: bool,

    /// Resume a specific session by id, keeping its conversation history.
    #[arg(long, value_name = "SESSION_ID")]
    pub resume: Option<String>,

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

    /// Which prior session (if any) to continue. An explicit `--resume <id>`
    /// wins over `--continue` (latest).
    pub fn resume_spec(&self) -> Option<ResumeSpec> {
        if let Some(id) = &self.resume {
            Some(ResumeSpec::Id(id.clone()))
        } else if self.continue_latest {
            Some(ResumeSpec::Latest)
        } else {
            None
        }
    }
}

/// Which prior session to continue.
#[derive(Debug, Clone)]
pub enum ResumeSpec {
    /// The most recent session in the worktree (the `LATEST` pointer).
    Latest,
    /// A specific session id.
    Id(String),
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

/// `cowboy mcp add --transport …`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Transport {
    /// A local subprocess speaking MCP over stdio.
    Stdio,
    /// A remote server over streamable HTTP / SSE.
    Http,
}

/// `cowboy models add --reasoning …`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Reasoning {
    /// Send no reasoning-effort hint at all.
    None,
    Minimal,
    Low,
    Medium,
    High,
}

impl Reasoning {
    /// The core value, where "no hint" is `None` rather than a variant.
    pub fn effort(self) -> Option<cowboy_core::config::ReasoningEffort> {
        use cowboy_core::config::ReasoningEffort as E;
        match self {
            Reasoning::None => None,
            Reasoning::Minimal => Some(E::Minimal),
            Reasoning::Low => Some(E::Low),
            Reasoning::Medium => Some(E::Medium),
            Reasoning::High => Some(E::High),
        }
    }
}

/// `cowboy artifact add --kind …`.
///
/// Deliberately narrower than [`cowboy_core::artifact::ArtifactKind`]: `handoff` and
/// `decision_record` are published by the agent and the session machinery, so offering
/// them here would invite a hand-written artifact that downstream code expects to have
/// been generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Kind {
    Contract,
    Summary,
    Patch,
    Diff,
    #[value(name = "test-result", alias = "test_result")]
    TestResult,
    Notes,
    Review,
    Other,
}

impl Kind {
    pub fn artifact_kind(self) -> cowboy_core::artifact::ArtifactKind {
        use cowboy_core::artifact::ArtifactKind as K;
        match self {
            Kind::Contract => K::Contract,
            Kind::Summary => K::Summary,
            Kind::Patch => K::Patch,
            Kind::Diff => K::Diff,
            Kind::TestResult => K::TestResult,
            Kind::Notes => K::Notes,
            Kind::Review => K::Review,
            Kind::Other => K::Other,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create initial project config files under `.cowboy/`.
    Init(InitArgs),

    /// Check kernel prerequisites, model config, and the egress gateway.
    Doctor,

    /// Inspect the sandbox boundary for this project.
    #[command(after_help = "\
Examples:
  cowboy sandbox plan          # what the agent can read, write and reach
  cowboy sandbox exec cargo test

`plan` is the honest answer to \"what is the agent allowed to do here?\" — it is rendered
from the same pure logic the session builds the boundary from, not a separate summary.")]
    Sandbox(SandboxArgs),

    /// Let the sandbox see a host path outside this project.
    ///
    /// Takes effect for the next command, including in a session already running.
    /// Credential stores (~/.aws, ~/.ssh, browser profiles, …) are always refused —
    /// use `cowboy secrets add` for those.
    #[command(after_help = "\
Examples:
  cowboy grant ~/src/shared-lib           # read-write, this project only
  cowboy grant --ro /opt/reference-data   # read-only
  cowboy grant --global ~/src/shared-lib  # every project on this machine
  cowboy grant --list                     # what is granted here
  cowboy grant --remove ~/src/shared-lib  # take it back")]
    Grant(GrantArgs),

    /// Open an interactive shell inside the agent sandbox.
    Shell,

    /// Run a command inside the agent sandbox.
    ///
    /// The same thing as `cowboy sandbox exec`, which spells out that the command is
    /// confined; this is the short form you actually type.
    #[command(
        alias = "exec",
        after_help = "\
Examples:
  cowboy run cargo test  # run it under the same confinement the agent gets
  cowboy run -- ls -la   # use -- when the command has its own flags

There is no network unless the project's security.yaml allows the destination."
    )]
    Run {
        /// The command and its arguments.
        #[arg(trailing_var_arg = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },

    /// Patch helper (wraps git inside the sandbox).
    #[command(after_help = "\
Examples:
  cowboy patch show   # the working-tree diff
  cowboy patch save   # write it to .cowboy/diff.patch
  cowboy patch revert # discard uncommitted tracked changes (asks first)

The workspace is bind-mounted, so the agent's edits are already in your real working
tree — commit them with plain git.")]
    Patch(PatchArgs),

    /// Inspect the session's long-running processes (the agent starts them).
    Proc(ProcArgs),

    /// Configure model providers (home-owned) and models. With no subcommand,
    /// show the current configuration and effective default.
    #[command(after_help = "\
Examples:
  cowboy models                        # what is configured, and the effective default
  cowboy models setup                  # guided provider + model setup
  cowboy models available              # what your endpoint actually offers
  cowboy models use claude-sonnet-4-6  # set the project default

Credentials live only in ~/.config/cowboy/providers.yaml (mode 0600) and are read
host-side. They are never written into a project or bound into the sandbox.")]
    Models(ModelsArgs),

    /// List the external agent CLIs the crew can delegate to (grok, …) and whether
    /// each is installed and logged in.
    #[command(after_help = "\
Harnesses are configured in ~/.config/cowboy/harnesses.yaml (user-level only):

  harnesses:
    grok:
      kind: grok
      model: grok-4.7        # optional; the CLI's default otherwise
      auth: auth_file        # or full_home — the login plus your grok config

Route a crew category to one in crew.yaml (`exploration: grok`), or ask the agent to
\"have grok …\". A harness runs inside cowboy's sandbox on your subscription.")]
    Harnesses,

    /// List or show agent skills (reusable instructions under .cowboy/skills/).
    #[command(alias = "skills")]
    Skill(SkillArgs),

    /// List or show agent definitions (specialist personas under .claude/agents/).
    #[command(alias = "agent")]
    Agents(AgentsArgs),

    /// End this project's running sessions and release their sandboxes.
    Down(DownArgs),

    /// Serve a web UI to attach to running sessions from a browser (e.g. a phone
    /// over Tailscale). Binds loopback by default; token-authenticated.
    #[command(after_help = "\
Examples:
  cowboy web on                          # loopback only
  cowboy web on --bind 100.x.y.z:7777    # a Tailscale address
  cowboy web status                      # the URL, plus a QR code for a remote bind
  cowboy web off")]
    Web(WebArgs),

    /// Attach the TUI to a running session (by id, or a worker socket path).
    Attach {
        #[arg(value_name = "SESSION")]
        target: String,
    },

    /// List sessions tracked by the daemon.
    ///
    /// A shortcut for `cowboy session list`.
    Sessions {
        /// Merge daemon-known sessions from every project with on-disk history
        /// from the current project. Still works when the daemon is unavailable.
        #[arg(long)]
        all: bool,
    },

    /// Inspect and maintain sessions (list, reap stale records and their leases).
    Session(SessionCmdArgs),

    /// List or create git worktrees for parallel sessions.
    #[command(
        alias = "worktrees",
        after_help = "\
Examples:
  cowboy worktree create \"fix login\"      # make cowboy/fix-login and a branch for it
  cowboy worktree list                    # which worktree each session is holding
  cowboy worktree status cowboy/fix-login # is it mergeable into HEAD?
  cowboy worktree diff --session 1788401869978-1

Running `cowboy` inside a worktree confines the agent to that worktree, so two sessions
can work the same repo without stepping on each other."
    )]
    Worktree(WorktreeArgs),

    /// Inspect the agent's saved memory (project + global).
    #[command(alias = "memories")]
    Memory(MemoryCmdArgs),

    /// Grant host credentials (gh, gcloud, kubectl, …) into the sandbox.
    #[command(alias = "secret")]
    Secrets(SecretsCmdArgs),

    /// Configure MCP servers the agent can discover and call (host-owned).
    Mcp(McpCmdArgs),

    /// Inspect or publish session artifacts (contracts, summaries, handoffs, …).
    #[command(alias = "artifacts")]
    Artifact(ArtifactCmdArgs),

    /// Print a session's handoff summary (defaults to the most recent).
    Handoff {
        #[arg(value_name = "SESSION")]
        session: Option<String>,
    },

    /// List or show decisions recorded in a session.
    #[command(alias = "decision")]
    Decisions(DecisionsCmdArgs),

    /// Send a structured message to a session inbox (daemon-mediated bus).
    #[command(after_help = "\
Examples:
  cowboy message \"the API contract changed\" --to 1788401869978-1
  cowboy message \"pausing for a release\" --all")]
    Message {
        /// The message text.
        message: String,
        /// Target session id.
        #[arg(long)]
        to: Option<String>,
        /// Broadcast to all other sessions instead of one.
        #[arg(long)]
        all: bool,
    },

    /// Read a session's message inbox (defaults to the most recent). Reading
    /// drains the inbox unless --peek is given.
    Inbox {
        #[arg(value_name = "SESSION")]
        session: Option<String>,

        /// Show the messages without consuming them.
        #[arg(long)]
        peek: bool,
    },

    /// Read-only review of a session's output (or a branch): prints a bundle
    /// and records a Review artifact. Never edits anything.
    Review {
        #[arg(value_name = "SESSION")]
        session: Option<String>,
        /// Review a branch's changes instead of a session.
        #[arg(long)]
        branch: Option<String>,
    },

    /// Create or inspect Ranch Plans (multi-workstream tasks).
    #[command(after_help = "\
A ranch splits one large task into dependency-aware workstreams, each a normal session in
its own worktree and branch. The usual arc:

  cowboy ranch plan \"migrate to the new auth service\"  # an agent proposes the workstreams
  cowboy ranch status my-ranch                         # review the plan it drafted
  cowboy ranch start my-ranch                          # launch whatever is ready
  cowboy ranch watch my-ranch                          # live dashboard
  cowboy ranch accept my-ranch api-layer               # sign off a gated workstream

`plan` reads the codebase and starts nothing, so the plan is yours to edit first.
`ranch draft <spec>` is the lower-level form the agent itself uses.")]
    Ranch(RanchArgs),

    /// Manage the Crew Roster (route delegated work to models by category/effort).
    Crew(CrewArgs),

    /// List session logs.
    Logs,

    /// Replay or inspect a previous session.
    Replay {
        #[arg(value_name = "SESSION_ID")]
        session_id: String,
        /// Browse the terminal event journal in a read-only TUI.
        #[arg(long)]
        tui: bool,
    },

    /// Print a shell completion script.
    ///
    /// Completions matter more here than in most CLIs: the command tree is wide, and
    /// several arguments are ids you would otherwise copy by hand.
    #[command(after_help = "\
Examples:
  cowboy completions zsh  > \"${fpath[1]}/_cowboy\"
  cowboy completions bash > ~/.local/share/bash-completion/completions/cowboy
  cowboy completions fish > ~/.config/fish/completions/cowboy.fish")]
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Internal: in-sandbox worker for the structured file tools (reads a JSON
    /// request on stdin). Not for direct use.
    #[command(name = "x-fileop", hide = true)]
    XFileop,

    /// Internal: the MCP server an external harness (grok, …) is given, relaying
    /// `ask_foreman`/`report_progress` to the foreman over `socket`. Not for direct use.
    #[command(name = "x-foreman-mcp", hide = true)]
    XForemanMcp { socket: std::path::PathBuf },

    /// Internal: the in-sandbox shim that applies Landlock + seccomp then execs
    /// the agent's command. Reads its request from stdin as JSON.
    #[command(name = "x-sandbox-shim", hide = true)]
    XSandboxShim,

    /// Internal: holds a session's namespaces open. Runs inside them, brings
    /// loopback up, and exits when its stdin closes.
    #[command(name = "x-sandbox-holder", hide = true)]
    XSandboxHolder,

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
    /// Continue a prior session: load its transcript as the starting history.
    #[arg(long)]
    pub resume: Option<String>,
    /// Tag this session as a Ranch workstream.
    #[arg(long)]
    pub ranch_id: Option<String>,
    /// Which workstream of `--ranch-id` this session is running.
    #[arg(long)]
    pub workstream_id: Option<String>,
}

#[derive(Debug, Args)]
pub struct SandboxArgs {
    #[command(subcommand)]
    pub command: SandboxCommand,
}

/// `cowboy grant` — record a runtime path grant.
#[derive(Debug, Args)]
pub struct GrantArgs {
    /// The host path to grant. Omit with `--list`.
    #[arg(value_name = "PATH")]
    pub path: Option<String>,

    /// Grant read-only access. The default is read-write, since a path you ask for
    /// by hand is usually one you intend to work in.
    #[arg(long)]
    pub ro: bool,

    /// Remember for every project on this machine, not just this one.
    #[arg(long)]
    pub global: bool,

    /// Forget a previously granted path.
    #[arg(long)]
    pub remove: bool,

    /// Show the saved grants for this project.
    #[arg(long)]
    pub list: bool,
}

#[derive(Debug, Subcommand)]
pub enum SandboxCommand {
    /// Print the confinement plan for this project: what the agent can read,
    /// write, and reach, and which paths can never be granted at runtime.
    Plan,

    /// Run one command inside the sandbox, with no network access.
    Exec {
        /// The command and its arguments.
        #[arg(trailing_var_arg = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Overwrite existing config files if present (asks first).
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
    /// Save the current git diff to `.cowboy/diff.patch`.
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
    /// End sessions for EVERY project, not just this one (asks first).
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, Args)]
pub struct WebArgs {
    #[command(subcommand)]
    pub command: WebCommand,
}

#[derive(Debug, Subcommand)]
pub enum WebCommand {
    /// Enable the web UI and have the daemon start serving it.
    On {
        /// Address to bind, e.g. `127.0.0.1:8787` or your Tailscale IP
        /// `100.x.y.z:8787`. Persisted; defaults to `127.0.0.1:8787`.
        /// Non-loopback/non-Tailscale binds are refused unless `--lan` is set.
        #[arg(long)]
        bind: Option<String>,
        /// Permit a non-loopback, non-Tailscale bind (LAN / `0.0.0.0`). The token
        /// then travels in cleartext — only use on a trusted network.
        #[arg(long)]
        lan: bool,
    },
    /// Disable the web UI and stop the daemon serving it.
    Off,
    /// Show whether the web UI is enabled + serving, with its URL (and a QR for
    /// a remote bind).
    Status,
}

#[derive(Debug, Args)]
#[command(
    after_help = "Run `cowboy models` with no subcommand to show the current configuration.\n\
Use `cowboy models setup` for guided provider and model setup."
)]
pub struct ModelsArgs {
    #[command(subcommand)]
    pub command: Option<ModelsCommand>,
}

#[derive(Debug, Subcommand)]
pub enum ModelsCommand {
    /// Guided provider and model setup. Validates everything before replacing
    /// the two home-owned files, and asks before replacing an existing entry.
    #[command(after_help = "\
Setup first tries the endpoint catalogue for 8 seconds, then falls back to a manual\n\
model id without exposing credentials or raw server responses. Existing malformed files\n\
are never overwritten; repair them first. Advanced tuning is optional.")]
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
    #[command(after_help = "\
Examples:
  cowboy models add anthropic/claude-sonnet-4-6
  cowboy models add cerebras/zai-glm-4.7 --name fast --default
  cowboy models add openai/gpt-5 --reasoning high --max-output 32000

Shipped defaults fill in temperature, context window and pricing for known ids;
`cowboy models available` lists what your endpoint actually offers.")]
    Add {
        /// The provider-side model id, e.g. `cerebras/zai-glm-4.7`.
        id: String,
        /// Friendly name (config key). Defaults to the recommended name.
        #[arg(long)]
        name: Option<String>,
        /// Provider to use (defaults to the only configured one).
        #[arg(long)]
        provider: Option<String>,
        /// Sampling temperature (provider default if omitted).
        #[arg(long, value_name = "FLOAT")]
        temp: Option<f32>,
        /// Context window in tokens, used to size the /context gauge and to decide
        /// when to compact.
        #[arg(long, value_name = "TOKENS")]
        context: Option<u32>,
        /// Cap on tokens generated per response.
        #[arg(long = "max-output", value_name = "TOKENS")]
        max_output: Option<u32>,
        /// Reasoning effort to request. `none` sends no hint at all.
        #[arg(long, value_enum)]
        reasoning: Option<Reasoning>,
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
    /// List sessions tracked by the daemon (same as `cowboy sessions`).
    List {
        /// Merge daemon-known sessions from every project with on-disk history
        /// from the current project. Still works when the daemon is unavailable.
        #[arg(long)]
        all: bool,
    },
    /// Reap stale (crashed/abandoned) session records and release their leases.
    /// Worktrees and branches are never touched.
    Cleanup {
        /// Show what would be reaped without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Args)]
pub struct SecretsCmdArgs {
    #[command(subcommand)]
    pub command: SecretsCommand,
}

#[derive(Debug, Subcommand)]
pub enum SecretsCommand {
    /// Show configured credential grants and whether each host source exists.
    List,
    /// Add a credential grant (a known preset and/or explicit env/file grants) to
    /// your personal host-side overlay; --repo prints a snippet to paste instead.
    #[command(after_help = "\
Examples:
  cowboy secrets add gh                       # a known preset (gh, gcloud, kubectl, aws, git, ssh)
  cowboy secrets add --env GITHUB_TOKEN       # pass a host env var through by name
  cowboy secrets add --env TOKEN=MY_HOST_VAR  # ...under a different name inside
  cowboy secrets add --file ~/.netrc          # bind a host file read-only
  cowboy secrets add gh --global              # every project, not just this one
  cowboy secrets add gh --repo                # print a security.yaml snippet instead of writing

Values are resolved host-side. The overlay lives in ~/.config/cowboy/secrets/, which the
agent cannot write.")]
    Add(SecretsAddArgs),
}

#[derive(Debug, Args)]
pub struct SecretsAddArgs {
    /// A known tool preset: gh, gcloud, kubectl, aws, git, ssh.
    pub preset: Option<String>,
    /// Grant an env var into the sandbox: `NAME` or `NAME=HOST_ENV`.
    #[arg(long = "env", value_name = "NAME[=HOST_ENV]")]
    pub env: Vec<String>,
    /// Grant a host file/dir read-only: `SRC` or `SRC:CONTAINER_TARGET`.
    #[arg(long = "file", value_name = "SRC[:TARGET]")]
    pub file: Vec<String>,
    /// Write to the cross-project user overlay instead of this worktree's.
    #[arg(long)]
    pub global: bool,
    /// Print a snippet to paste into the repo's .cowboy/security.yaml instead of
    /// writing your personal (home-dir) overlay.
    #[arg(long)]
    pub repo: bool,
}

#[derive(Debug, Args)]
pub struct McpCmdArgs {
    #[command(subcommand)]
    pub command: McpCommand,
}

#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// List configured MCP servers (name, transport, enabled).
    List,
    /// Show one server's full configuration.
    Show { name: String },
    /// Add or replace an MCP server in ~/.config/cowboy/mcp.yaml.
    #[command(after_help = "\
Examples:
  cowboy mcp add filesystem --transport stdio --command npx --arg -y --arg @modelcontextprotocol/server-filesystem --arg /workspace --description \"files under /workspace\" --tool \"*\"
  cowboy mcp add docs --transport http --url https://mcp.example.com/sse --header \"Authorization=Bearer ${TOKEN}\" --tool search

--tool is fail-closed: with none given the server is configured but exposes nothing.
Pass --tool '*' to expose everything, or name each tool. Check the result with
`cowboy mcp test <name>`.")]
    Add(McpAddArgs),
    /// Remove an MCP server.
    Remove { name: String },
    /// Enable a configured server.
    Enable { name: String },
    /// Disable a server (kept in config, not connected).
    Disable { name: String },
    /// Connect to a server and list its tools (a connectivity check).
    Test { name: String },
    /// Trust this repo's `.mcp.json` servers (review + approve them; required
    /// before the agent can use repo-defined servers). Re-run if the file changes.
    Trust,
    /// Revoke trust for this repo's `.mcp.json` servers.
    Untrust,
}

#[derive(Debug, Args)]
pub struct McpAddArgs {
    /// Local name for the server (e.g. `linear`, `filesystem`).
    pub name: String,
    /// Transport: `stdio` (local subprocess) or `http` (remote endpoint).
    #[arg(long, value_enum)]
    pub transport: Transport,
    /// One-line description shown to the agent (e.g. "issue tracking").
    #[arg(long)]
    pub description: Option<String>,
    /// stdio: the command to run (e.g. `npx`).
    #[arg(long)]
    pub command: Option<String>,
    /// stdio: an argument to the command (repeatable, in order). Leading-dash
    /// values are fine (e.g. `--arg -y`).
    #[arg(long = "arg", value_name = "ARG", allow_hyphen_values = true)]
    pub args: Vec<String>,
    /// stdio: an environment variable, `KEY=VALUE` (repeatable). Use `${VAR}` in
    /// VALUE to reference host env; never inline secret literals.
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    /// http: the server URL.
    #[arg(long)]
    pub url: Option<String>,
    /// http: a request header, `KEY=VALUE` (repeatable). Use `${VAR}` in VALUE.
    #[arg(long = "header", value_name = "KEY=VALUE")]
    pub header: Vec<String>,
    /// Tool names to expose (repeatable), fail-closed: omit to expose NONE, or pass
    /// `--tool '*'` to expose all of the server's tools.
    #[arg(long = "tool", value_name = "NAME")]
    pub tools: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ArtifactCmdArgs {
    #[command(subcommand)]
    pub command: ArtifactCommand,
}

#[derive(Debug, Subcommand)]
pub enum ArtifactCommand {
    /// List artifacts for a session (defaults to the most recent).
    List {
        #[arg(value_name = "SESSION")]
        session: Option<String>,
    },
    /// Print an artifact's body by id.
    Show {
        /// The artifact id, as shown by `cowboy artifact list`.
        id: String,
        /// Session to read from (defaults to the most recent in this worktree).
        #[arg(long, value_name = "SESSION")]
        session: Option<String>,
    },
    /// Publish a file as a session artifact.
    Add {
        /// Path to the file to publish.
        path: String,
        /// What sort of artifact this is (defaults to `notes`).
        #[arg(long, value_enum)]
        kind: Option<Kind>,
        /// Friendly title (defaults to the file name).
        #[arg(long)]
        title: Option<String>,
        /// One-line summary.
        #[arg(long)]
        summary: Option<String>,
        /// Session to publish into (defaults to the most recent in this worktree).
        #[arg(long, value_name = "SESSION")]
        session: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct CrewArgs {
    #[command(subcommand)]
    pub command: CrewCommand,
}

#[derive(Debug, Subcommand)]
pub enum CrewCommand {
    /// Write a default crew roster (tiers derived from your models' prices).
    Init {
        /// Overwrite an existing crew.yaml (asks first).
        #[arg(long)]
        force: bool,
    },
    /// Show the routing matrix (category × effort → model).
    List,
    /// Print the full crew.yaml (roster + delegation rules).
    Show,
    /// Check the roster (models exist, `general` defined, etc.).
    Validate,
    /// Show recorded delegation usage per model (tasks, success %, avg duration).
    Usage,
    /// Suggest roster changes from recorded outcomes (recommend-only; never edits).
    Recommend,
}

#[derive(Debug, Args)]
pub struct RanchArgs {
    #[command(subcommand)]
    pub command: RanchCommand,
}

#[derive(Debug, Subcommand)]
pub enum RanchCommand {
    /// Create a new ranch plan (writes a skeleton ranch.yaml to fill in).
    Create {
        /// The ranch's title (also seeds its id).
        title: String,
        /// The overall goal.
        #[arg(long)]
        goal: Option<String>,
    },
    /// Add a workstream to a ranch (no hand-editing ranch.yaml). Rejects a
    /// dependency cycle or an unknown `--depends-on` id.
    Add {
        #[arg(value_name = "RANCH")]
        id: String,
        /// Workstream id (short, unique within the ranch).
        #[arg(value_name = "WORKSTREAM_ID")]
        workstream: String,
        /// What this workstream should accomplish.
        #[arg(long)]
        goal: String,
        /// Display title (defaults to the workstream id).
        #[arg(long)]
        title: Option<String>,
        /// Workstream ids this one depends on (comma-separated).
        #[arg(long, value_delimiter = ',')]
        depends_on: Vec<String>,
        /// Acceptance criteria, human-readable (comma-separated).
        #[arg(long, value_delimiter = ',')]
        acceptance: Vec<String>,
        /// Expected artifact name(s) this workstream should publish (repeatable).
        #[arg(long = "expects", value_name = "ARTIFACT")]
        expects: Vec<String>,
    },
    /// Decompose a goal into a ranch plan with the agent: it researches the
    /// codebase read-only and proposes workstreams + dependencies for you to
    /// review (writes a draft ranch.yaml; starts nothing).
    Plan {
        /// The overall goal to decompose into workstreams.
        goal: String,
    },
    /// Draft a ranch from a decomposition spec file (YAML/JSON with `title`,
    /// `goal`, and a `workstreams` list). Validates the dependency DAG and writes
    /// the draft ranch.yaml. Used by `cowboy ranch plan`: the agent authors the
    /// spec with the `write` tool, then runs this to validate and draft it.
    Draft {
        /// Path to the decomposition spec (YAML or JSON).
        #[arg(value_name = "SPEC")]
        spec: String,
    },
    /// Show ranch status: all ranches, or one with its workstreams.
    Status {
        #[arg(value_name = "RANCH")]
        id: Option<String>,
    },
    /// Launch ready workstreams (deps complete), each in its own worktree/branch.
    /// Re-run as workstreams finish to advance the plan.
    Start {
        #[arg(value_name = "RANCH")]
        id: String,
    },
    /// Attach the TUI to a workstream's running session.
    Attach {
        #[arg(value_name = "RANCH")]
        id: String,
        #[arg(value_name = "WORKSTREAM")]
        workstream: String,
    },
    /// Mark a workstream complete (promotes its artifacts + unblocks dependents).
    Complete {
        #[arg(value_name = "RANCH")]
        id: String,
        #[arg(value_name = "WORKSTREAM")]
        workstream: String,
    },
    /// Sign off on a workstream waiting at its acceptance gate (unblocks deps).
    Accept {
        #[arg(value_name = "RANCH")]
        id: String,
        #[arg(value_name = "WORKSTREAM")]
        workstream: String,
    },
    /// Reset a failed or interrupted workstream so it re-runs on the next start.
    Retry {
        #[arg(value_name = "RANCH")]
        id: String,
        #[arg(value_name = "WORKSTREAM")]
        workstream: String,
    },
    /// Live TUI dashboard: watch workstreams advance, start/refresh from keys.
    Watch {
        #[arg(value_name = "RANCH")]
        id: String,
    },
    /// Propose a scope change to the plan (recorded as pending; needs approval).
    Propose {
        #[arg(value_name = "RANCH")]
        id: String,
        /// One-line summary of the proposal.
        #[arg(long)]
        summary: String,
        /// Why this change is needed.
        #[arg(long)]
        rationale: Option<String>,
        /// Propose adding a workstream with this id.
        #[arg(long, value_name = "WORKSTREAM_ID", group = "change")]
        add_workstream: Option<String>,
        /// Propose removing this (not-yet-started) workstream.
        #[arg(long, value_name = "WORKSTREAM_ID", group = "change")]
        remove_workstream: Option<String>,
        /// File a free-form note/concern (no automatic edit).
        #[arg(long, group = "change")]
        note: bool,
        /// Title for an added workstream.
        #[arg(long)]
        title: Option<String>,
        /// Goal for an added workstream.
        #[arg(long)]
        goal: Option<String>,
        /// Dependencies for an added workstream (comma-separated ids).
        #[arg(long, value_delimiter = ',')]
        depends_on: Vec<String>,
    },
    /// List a ranch's scope-change proposals.
    Proposals {
        #[arg(value_name = "RANCH")]
        id: String,
        /// Include already-decided proposals (default: pending only).
        #[arg(long)]
        all: bool,
    },
    /// Approve a pending proposal: apply its change to the plan.
    Approve {
        #[arg(value_name = "RANCH")]
        id: String,
        #[arg(value_name = "PROPOSAL")]
        proposal: String,
    },
    /// Reject a pending proposal (records the decision; plan unchanged).
    Reject {
        #[arg(value_name = "RANCH")]
        id: String,
        #[arg(value_name = "PROPOSAL")]
        proposal: String,
        /// Why it was rejected. Recorded with the decision and shown to the
        /// workstream that proposed it, so it can try something else.
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct DecisionsCmdArgs {
    #[command(subcommand)]
    pub command: DecisionsCommand,
}

#[derive(Debug, Subcommand)]
pub enum DecisionsCommand {
    /// List recorded decisions (defaults to the most recent session).
    List {
        #[arg(value_name = "SESSION")]
        session: Option<String>,
    },
    /// Show one decision by id.
    Show {
        /// The decision id, as shown by `cowboy decisions list`.
        id: String,
        /// Session to read from (defaults to the most recent in this worktree).
        #[arg(long, value_name = "SESSION")]
        session: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct MemoryCmdArgs {
    #[command(subcommand)]
    pub command: MemoryCommand,
}

#[derive(Debug, Subcommand)]
pub enum MemoryCommand {
    /// List saved memories (project + global) for the current worktree.
    List,
    /// Print a memory's full body by name.
    Show { name: String },
    /// Delete a memory by name (shows it, then asks).
    Delete { name: String },
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
    /// Show a branch's diff stat vs its fork point (read-only).
    Diff {
        /// Branch to inspect (or use --session).
        #[arg(value_name = "BRANCH")]
        branch: Option<String>,
        /// Resolve the branch from a session id instead.
        #[arg(long, value_name = "SESSION")]
        session: Option<String>,
    },
    /// Summarize a branch's changes + mergeability vs HEAD (read-only).
    Status {
        /// Branch to inspect (or use --session).
        #[arg(value_name = "BRANCH")]
        branch: Option<String>,
        /// Resolve the branch from a session id instead.
        #[arg(long, value_name = "SESSION")]
        session: Option<String>,
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

#[derive(Debug, Args)]
pub struct AgentsArgs {
    #[command(subcommand)]
    pub command: AgentsCommand,
}

#[derive(Debug, Subcommand)]
pub enum AgentsCommand {
    /// List available agent definitions (name + description + model).
    List,
    /// Print an agent's definition (its system prompt / review approach).
    Show { name: String },
}

#[derive(Debug, Subcommand)]
pub enum ProcCommand {
    /// List configured processes and their status.
    List,
    /// Explain why a process cannot be started from here (they are session-owned).
    Start { name: String },
    /// Stop a process by name (only reaches a stale one; session processes end with
    /// the session).
    Stop { name: String },
    /// Restart a process by name.
    Restart { name: String },
    /// Stream logs for a process.
    Logs { name: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A subagent task that starts with `-` must survive the child's clap parse. The
    /// parent spawns `cowboy -- <task>` (see `exec_subagent`); the `--` makes clap
    /// treat the hyphen-leading string as the positional TASK, not a flag — otherwise
    /// a task like "-v refactor" or "--wip" fails the subagent to start.
    #[test]
    fn a_hyphen_leading_task_parses_as_the_positional_after_dashdash() {
        let cli = Cli::try_parse_from(["cowboy", "--", "-v refactor the parser"])
            .expect("`cowboy -- <task>` should parse");
        assert_eq!(cli.task.as_deref(), Some("-v refactor the parser"));

        let cli = Cli::try_parse_from(["cowboy", "--", "--wip"]).expect("parse");
        assert_eq!(cli.task.as_deref(), Some("--wip"));

        // An ordinary task still parses with or without the separator.
        let cli = Cli::try_parse_from(["cowboy", "fix the tests"]).unwrap();
        assert_eq!(cli.task.as_deref(), Some("fix the tests"));
    }

    #[test]
    fn session_browser_and_tui_replay_flags_parse_additively() {
        let bare = Cli::try_parse_from(["cowboy", "sessions"]).unwrap();
        assert!(matches!(
            bare.command,
            Some(Command::Sessions { all: false })
        ));
        let all = Cli::try_parse_from(["cowboy", "session", "list", "--all"]).unwrap();
        assert!(matches!(
            all.command,
            Some(Command::Session(SessionCmdArgs {
                command: SessionCommand::List { all: true }
            }))
        ));
        let replay = Cli::try_parse_from(["cowboy", "replay", "1789651", "--tui"]).unwrap();
        assert!(matches!(
            replay.command,
            Some(Command::Replay {
                session_id,
                tui: true
            }) if session_id == "1789651"
        ));
    }

    #[test]
    fn bare_models_is_a_valid_discovery_command() {
        let cli = Cli::try_parse_from(["cowboy", "models"]).expect("bare models parses");
        assert!(matches!(
            cli.command,
            Some(Command::Models(ModelsArgs { command: None }))
        ));
    }
}
