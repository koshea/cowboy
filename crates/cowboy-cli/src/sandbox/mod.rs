//! The sandbox seam: how the agent's shell commands are confined and executed.
//!
//! The agent loop runs **host-side** in the worker process; the sandbox is the
//! jail for the commands it asks to run. [`Sandbox`] is the boundary between the
//! two, so the loop never knows which confinement mechanism is in use.
//!
//! [`native::NativeSandbox`] is the one implementation. Everything about it that
//! does not depend on the kernel — grants, the per-command plan, background process
//! bookkeeping, the policy engine — is shared; what confines a command is chosen per
//! OS by [`backend`]:
//!
//! - **Linux** ([`linux`]): namespaces held by a session process, bwrap per command,
//!   Landlock + seccomp applied by the shim, egress through an nft-intercepted relay.
//! - **macOS** ([`macos`]): a Seatbelt profile applied by the shim with
//!   `sandbox_init`, egress through a per-session authenticated proxy on the host.
//!
//! Both fail closed, and both keep every policy decision on the host side.

pub mod attribution;
pub mod exec;
pub mod grants;
pub mod native;
pub mod policy;
pub mod preflight;
pub mod shim;
pub mod stream;

#[cfg(target_os = "linux")]
pub mod bwrap;
#[cfg(target_os = "linux")]
pub mod cgroup;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub mod lockdown;
#[cfg(target_os = "linux")]
pub mod session;
#[cfg(target_os = "linux")]
pub mod transport;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("cowboy's sandbox supports Linux and macOS only");

/// The confinement mechanism for this OS: a `Session` type with the same surface on
/// every platform, so [`native::NativeSandbox`] never names one.
#[cfg(target_os = "linux")]
pub(crate) use linux as backend;
#[cfg(target_os = "macos")]
pub(crate) use macos as backend;

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;

/// Loopback port inside the sandbox where the egress relay accepts intercepted
/// TCP (Linux). Fixed rather than per-session: it is only ever reachable from inside one
/// network namespace, so there is nothing for a unique port to protect against,
/// and a constant keeps the Landlock rule and the nft rule obviously in agreement.
pub const RELAY_PORT: u16 = 8443;

/// Loopback port inside the sandbox where the relay accepts DNS queries.
pub const DNS_PORT: u16 = 5354;

/// Re-exported so the agent loop can recognise the two outcomes that are not a real
/// exit status and explain them, instead of handing the model a bare `124`.
pub(crate) use exec::{EXIT_CANCELLED, EXIT_TIMEOUT};

/// Result of a command execution inside the sandbox.
///
/// Lives here rather than beside the Docker client because it is part of the
/// sandbox contract, not of any one backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecResult {
    pub exit_code: i32,
}

/// Where things are, as a command inside the sandbox sees them — for telling the
/// agent, which cannot find out any other way that does not cost it a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPaths {
    /// The project.
    pub workdir: String,
    /// Scratch space that lasts the session and is not the project.
    pub scratch: String,
}

impl Default for SandboxPaths {
    /// The Linux layout with the default config.
    fn default() -> Self {
        Self {
            workdir: "/workspace".to_string(),
            scratch: "/tmp".to_string(),
        }
    }
}

/// Sink for human-readable sandbox bring-up status lines.
///
/// Bring-up can take a noticeable amount of time on a cold start and otherwise
/// happens *silently* inside the first command's execution; the agent loop drains
/// this into `AgentUi::notice` so the user sees why nothing is streaming yet.
pub type StatusTx = tokio::sync::mpsc::UnboundedSender<String>;
pub type StatusRx = tokio::sync::mpsc::UnboundedReceiver<String>;

/// A confined execution environment for one project's agent commands.
///
/// Implementations own whatever lifecycle their mechanism needs (containers,
/// namespaces) and must bring it up lazily — callers invoke the `exec`/`run`
/// methods without first ensuring anything is running.
///
/// **Security note:** every method here runs *untrusted* input. The agent chooses
/// the command strings; nothing in this trait may rely on the agent behaving.
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// The host project root (the workspace bind source).
    fn root(&self) -> &Path;

    /// A stable name identifying this project's sandbox, for logs and teardown.
    fn session_name(&self) -> &str;

    /// Where the project and the session scratch appear to a command.
    fn paths(&self) -> SandboxPaths {
        SandboxPaths::default()
    }

    /// Attach a sink for bring-up progress, replacing any previous one. When no
    /// sink is attached, reporting is a no-op.
    fn status_channel(&mut self) -> StatusRx;

    /// Whether the workspace declares dev dependencies via mise, so the caller
    /// can run a *visible* toolchain install at session start rather than letting
    /// it silently delay the first request.
    fn has_mise_config(&self) -> bool;

    /// Bring the sandbox up if it is not already. Must **fail closed**: if
    /// enforcement cannot be established, return an error rather than yielding a
    /// usable-but-unconfined environment.
    async fn ensure_running(&self) -> Result<()>;

    /// Tear down the running sandbox to free its resources; the next command
    /// brings it back. Best-effort.
    async fn stop(&self);

    /// Run a shell command, streaming combined output to `chunks` as it arrives,
    /// interruptible via `cancel` and bounded by `timeout_secs` (0 = unbounded).
    /// On cancel or timeout the whole process group is killed. Returns the exit
    /// status and the accumulated output.
    async fn exec_stream(
        &self,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: u64,
        cancel: tokio_util::sync::CancellationToken,
        chunks: StatusTx,
    ) -> Result<(ExecResult, String)>;

    /// Run a shell command capturing combined output, bounded by `timeout_secs`
    /// (0 = unbounded). For short control commands.
    async fn run_capture(
        &self,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: u64,
    ) -> Result<(ExecResult, String)>;

    /// Run `argv` with inherited stdio, returning its exit status.
    async fn run(&self, argv: &[String]) -> Result<ExecResult>;

    /// Open an interactive shell, inheriting the terminal.
    async fn shell(&self) -> Result<ExecResult>;

    /// Execute a structured file operation, passing `payload` on stdin so
    /// multi-line content avoids shell quoting entirely.
    async fn fileop(&self, payload: &str) -> Result<(ExecResult, String)>;

    /// Start a long-running background process, confined exactly like any other
    /// command, with its combined output appended to a log file in the workspace.
    ///
    /// Part of the seam because a coding agent cannot do without it: testing a web
    /// service means starting it and then talking to it, and a `shell` call cannot
    /// hold a server — each one is a fresh sandbox whose whole process tree is reaped
    /// when the command returns. (`&` and `setsid` do not escape that: bwrap is PID 1
    /// of the command's own PID namespace, so the kernel kills everything in it when
    /// bwrap exits. `cowboy proc start` did exactly this and reported success for a
    /// process that was already dead.)
    ///
    /// The process is owned by the **session**: it shares the session's network
    /// namespace so later commands reach it on loopback, gets its own PID namespace so
    /// stopping it reaps precisely its own tree, and dies with the session — which is
    /// the right lifetime for a dev server, and the reason the caller is the worker
    /// rather than a short-lived CLI.
    async fn start_process(&self, name: &str, command: &str, cwd: Option<&str>) -> Result<()>;

    /// Stop one background process.
    async fn stop_process(&self, name: &str) -> Result<()>;

    /// Names of the background processes believed to be running.
    fn running_processes(&self) -> Vec<String>;

    /// Stop the managed background processes declared in `agent.yaml`.
    async fn stop_all_processes(&self) -> Result<()>;

    /// Grant access to a host path for subsequent commands, remembering it for
    /// `persistence`.
    ///
    /// Part of the seam because it is the capability the container could not offer:
    /// a container's mounts are fixed when it is created, so "let me at that folder"
    /// meant editing config and restarting. Implementations must refuse anything the
    /// credential denylist covers, whatever the caller says — the user's approval is
    /// not the control there, since the path may have been chosen by the model.
    fn add_grant(
        &self,
        path: &Path,
        read_only: bool,
        persistence: grants::Persistence,
    ) -> Result<()>;

    /// Paths granted beyond the configured mounts, and whether each is read-only.
    ///
    /// Lets a caller answer "do I already have this?" without asking the user again,
    /// and lets the boundary be reported without reaching for the implementation.
    fn granted_paths(&self) -> Vec<(PathBuf, bool)>;
}
