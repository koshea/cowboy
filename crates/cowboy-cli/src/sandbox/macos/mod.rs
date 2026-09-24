//! The macOS backend: a Seatbelt profile per command, applied by the shim, and an
//! authenticated proxy on the host as the only way out.
//!
//! There is no session process to hold anything: the session is the proxy, the
//! scratch directory, and the set of processes whose profile can read that scratch
//! directory (see [`reap`]).

pub mod preflight;
pub(crate) mod proxy;
pub(crate) mod reap;
pub mod seatbelt;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use cowboy_sandbox::SandboxPlan;
use tokio::process::Command;

use super::exec::Prepared;
use super::shim::ShimRequest;

pub(crate) use super::native::StartContext;

/// Proxy variables set for every command, upper- and lower-case: tools disagree
/// about which they read (curl only honours lower-case `http_proxy`).
const PROXY_VARS: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];

/// One session: its proxy, and what identifies its processes.
pub(crate) struct Session {
    proxy: proxy::Proxy,
    exe: PathBuf,
    loopback_ports: Vec<u16>,
    /// This session's scratch `TMPDIR`, canonical. Every command's profile can read it
    /// and no other session's can, which is what [`reap::sweep`] identifies them by.
    canary: PathBuf,
}

impl Session {
    pub fn start(ctx: StartContext<'_>) -> Result<Self> {
        let tmp = ctx.scratch.join("tmp");
        std::fs::create_dir_all(&tmp)
            .with_context(|| format!("creating the session scratch {}", tmp.display()))?;
        let canary = std::fs::canonicalize(&tmp)?;
        // A previous session with the same scratch (a crashed worker) may have left
        // escaped processes behind; they are this session's now, and unwanted.
        reap::sweep(&canary);
        if ctx.limits.memory_mib.is_some() || ctx.limits.cpus.is_some() {
            (ctx.report)(
                "resource limits are configured but macOS cannot enforce them per session; \
                 only build parallelism is applied."
                    .to_string(),
            );
        }
        let proxy = proxy::Proxy::start(ctx.engine)?;
        Ok(Self {
            proxy,
            exe: ctx.exe.to_path_buf(),
            loopback_ports: ctx.loopback_ports.to_vec(),
            canary,
        })
    }

    pub fn is_alive(&self) -> bool {
        self.proxy.is_alive()
    }

    /// Stop the proxy, then kill whatever is still running under this session's
    /// profile — including anything that `setsid`ed its way out of a command.
    pub fn stop(&mut self) {
        self.proxy.stop();
        let n = reap::sweep(&self.canary);
        if n > 0 {
            tracing::debug!(processes = n, "reaped processes left behind by the session");
        }
    }

    /// Confine `shell_command` according to `plan`.
    pub fn prepare(&self, plan: &SandboxPlan, shell_command: &str) -> Result<Prepared> {
        materialize_home_links(plan)?;
        let profile = cowboy_sandbox::seatbelt::profile(
            plan,
            &cowboy_sandbox::seatbelt::Network {
                proxy_port: self.proxy.port(),
                loopback_ports: &self.loopback_ports,
            },
            &canonical,
        );
        let credential = self.proxy.issue();
        let url = credential.url();

        let mut cmd = Command::new(&self.exe);
        cmd.arg("x-sandbox-shim");
        // A cleared environment, exactly as bwrap gives on Linux: nothing from the
        // worker (provider keys, `SSH_AUTH_SOCK`) leaks in by inheritance.
        cmd.env_clear();
        cmd.envs(plan.env.iter().map(|(k, v)| (k, v)));
        for var in PROXY_VARS {
            cmd.env(var, &url);
        }
        cmd.current_dir(&plan.workdir);
        // Not `process_group(0)`: the shim calls `setsid`, which a group leader may
        // not, and becomes the leader of a new group that way.
        cmd.stdin(Stdio::piped());

        let request = ShimRequest {
            command: shell_command.to_string(),
            seatbelt_profile: Some(profile),
            ..ShimRequest::default()
        };
        Ok(Prepared {
            cmd,
            request,
            ticket: Some(Box::new(credential)),
        })
    }

    /// The command that runs a structured file operation: the cowboy binary at its
    /// host path, which the plan exposes read-only.
    pub fn fileop_command(&self, exe: &Path) -> String {
        format!(
            "'{}' x-fileop",
            exe.to_string_lossy().replace('\'', r"'\''")
        )
    }
}

/// As on Linux, a session that is dropped without being stopped still takes its
/// processes with it.
impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Create the symlinks that stand in for credential binds in the agent's HOME.
///
/// Host-side, before the command, into a directory the agent can write: so a link is
/// replaced if it has been changed rather than trusted. Where it points is only ever
/// the plan's own (denylist-checked) source.
fn materialize_home_links(plan: &SandboxPlan) -> Result<()> {
    for (link, target) in &plan.home_links {
        if std::fs::read_link(link).is_ok_and(|t| &t == target) {
            continue;
        }
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(link);
        std::os::unix::fs::symlink(target, link)
            .with_context(|| format!("linking {} into the agent's HOME", link.display()))?;
    }
    Ok(())
}

/// The path Seatbelt will match: the real path, resolving through the deepest
/// existing ancestor for a path that does not exist yet (`/tmp/x` is checked as
/// `/private/tmp/x`).
fn canonical(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    let mut tail = Vec::new();
    let mut cur = p;
    while let Some(parent) = cur.parent() {
        if let Some(name) = cur.file_name() {
            tail.push(name.to_os_string());
        }
        if let Ok(c) = std::fs::canonicalize(parent) {
            return tail.iter().rev().fold(c, |acc, n| acc.join(n));
        }
        cur = parent;
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_path_canonicalizes_through_its_real_ancestor() {
        let got = canonical(Path::new("/tmp/cowboy-definitely-missing/x"));
        assert_eq!(
            got,
            PathBuf::from("/private/tmp/cowboy-definitely-missing/x")
        );
    }
}
