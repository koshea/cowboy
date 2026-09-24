//! The Linux backend: namespaces held for the session, bwrap per command, Landlock
//! and seccomp applied by the shim, and egress through the nft-intercepted relay.
//!
//! This module only adapts those pieces (`session`, `bwrap`, `exec`, `transport`) to
//! the backend surface [`crate::sandbox::native::NativeSandbox`] drives, so it holds
//! no mechanism of its own.

pub mod preflight;

use std::path::Path;

use anyhow::{Context, Result};
use cowboy_sandbox::SandboxPlan;

use super::bwrap::NetMode;
use super::exec::{self, Prepared};
use super::session::SessionSandbox;

pub(crate) use super::native::StartContext;

/// One session's namespaces, with the relay brokers already serving them.
pub(crate) struct Session {
    inner: SessionSandbox,
}

impl Session {
    /// Create the namespaces and start answering the relay.
    ///
    /// Fails closed: an error here means no command runs.
    pub fn start(ctx: StartContext<'_>) -> Result<Self> {
        let (inner, channels) = SessionSandbox::start(ctx.instance_key, ctx.exe, ctx.limits)?;
        match inner.limits_in_force() {
            Some(s) => (ctx.report)(format!("resource limits: {s}")),
            None if ctx.limits.memory_mib.is_some() || ctx.limits.cpus.is_some() => (ctx.report)(
                "resource limits are configured but cannot be enforced here (no delegated \
                 cgroup v2 subtree). Run `cowboy doctor` for details."
                    .to_string(),
            ),
            None => {}
        }
        // Serve both relay channels on dedicated threads. They must be running
        // before any command does, or the first connection blocks on a verdict
        // and the first lookup on a response.
        let handle = tokio::runtime::Handle::current();
        let engine = ctx.engine.clone();
        let connect = channels.connect;
        std::thread::Builder::new()
            .name("cowboy-egress-broker".into())
            .spawn({
                let handle = handle.clone();
                move || {
                    super::transport::broker::serve_blocking(connect, engine, handle);
                }
            })
            .context("spawning the egress policy broker")?;

        // The upstream resolver is read here, on the host, and the sandbox is
        // never told which one it is: it sends every query to a loopback port and
        // the answer comes back from this side.
        let upstream = cowboy_gateway::dns::host_resolver();
        let engine = ctx.engine;
        let resolve = channels.resolve;
        std::thread::Builder::new()
            .name("cowboy-dns-broker".into())
            .spawn(move || {
                super::transport::broker::serve_dns_blocking(resolve, engine, handle, upstream);
            })
            .context("spawning the dns policy broker")?;
        Ok(Self { inner })
    }

    /// Whether the namespaces still exist. A holder that died (OOM, external kill)
    /// must not be silently reused.
    pub fn is_alive(&self) -> bool {
        self.inner.is_alive()
    }

    /// Release the namespaces. Anything still inside loses its network.
    pub fn stop(&mut self) {
        self.inner.stop();
    }

    /// Confine `shell_command` according to `plan`, inside this session.
    ///
    /// `Inherit`: the session namespace is already entered, and unsharing here would
    /// discard the network namespace the transport is installed in.
    pub fn prepare(&self, plan: &SandboxPlan, shell_command: &str) -> Result<Prepared> {
        exec::build_command(plan, shell_command, NetMode::Inherit, Some(&self.inner))
    }

    /// The command that runs a structured file operation inside the sandbox: the
    /// in-sandbox cowboy binary, already bound read-only for the lockdown shim.
    pub fn fileop_command(&self, _exe: &Path) -> String {
        format!("{} x-fileop", cowboy_sandbox::SHIM_PATH)
    }
}
