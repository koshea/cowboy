//! The sandbox seam: how the agent's shell commands are confined and executed.
//!
//! The agent loop runs **host-side** in the worker process; the sandbox is the
//! jail for the commands it asks to run. [`Sandbox`] is the boundary between the
//! two, so the loop never knows which confinement mechanism is in use.
//!
//! Two implementations exist during the migration to host-native isolation:
//! `net::runtime::AgentRuntime` (Docker) and the namespace/Landlock sandbox that
//! replaces it. The trait is kept after Docker is removed because it is also the
//! seam the follow-up portability work plugs into.

pub mod bwrap;
pub mod exec;
pub mod shim;
pub mod stream;
pub mod transport;

use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;

/// Loopback port inside the sandbox where the egress relay accepts intercepted
/// TCP. Fixed rather than per-session: it is only ever reachable from inside one
/// network namespace, so there is nothing for a unique port to protect against,
/// and a constant keeps the Landlock rule and the nft rule obviously in agreement.
pub const RELAY_PORT: u16 = 8443;

/// Loopback port inside the sandbox where the relay accepts DNS queries.
pub const DNS_PORT: u16 = 5354;

/// Result of a command execution inside the sandbox.
///
/// Lives here rather than beside the Docker client because it is part of the
/// sandbox contract, not of any one backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecResult {
    pub exit_code: i32,
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

    /// Stop the managed background processes declared in `agent.yaml`.
    async fn stop_all_processes(&self) -> Result<()>;
}
