//! `cowboy proc ...` — supervise long-running processes defined in
//! `agent.yaml`. Processes run inside the agent container as detached process
//! groups; state (pid + logs) lives under `/workspace/.cowboy/proc/`.

use anyhow::{bail, Context, Result};
use cowboy_core::config::{AgentConfig, ConfigPaths, ProcessDef, SecurityConfig};

use crate::cli::{ProcArgs, ProcCommand};
use crate::net::docker::CliDocker;
use crate::net::runtime::AgentRuntime;

const CONTROL_TIMEOUT: u64 = 30;

struct Proc {
    runtime: AgentRuntime,
    procs: std::collections::BTreeMap<String, ProcessDef>,
    proc_dir: String,
    workdir: String,
    root: std::path::PathBuf,
}

#[derive(serde::Serialize)]
struct ProcessRecord<'a> {
    ts_ms: u128,
    name: &'a str,
    action: &'a str,
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub async fn run(args: ProcArgs) -> Result<()> {
    let root = crate::cmd::project_root()?;
    let paths = ConfigPaths::for_root(&root);
    let security = SecurityConfig::load(&paths.security)
        .context("loading .cowboy/security.yaml (run `cowboy init` first)")?;
    let agent_cfg = AgentConfig::load(&paths.agent).unwrap_or_default();
    let workdir = security.container.workdir.clone();
    let proc_dir = format!("{workdir}/.cowboy/proc");
    let runtime = AgentRuntime::new(Box::new(CliDocker::new()), root.clone(), security);

    let ctx = Proc {
        runtime,
        procs: agent_cfg.processes,
        proc_dir,
        workdir,
        root,
    };

    match args.command {
        ProcCommand::List => ctx.list().await,
        ProcCommand::Start { name } => ctx.start(&name).await,
        ProcCommand::Stop { name } => ctx.stop(&name).await,
        ProcCommand::Restart { name } => ctx.restart(&name).await,
        ProcCommand::Logs { name } => ctx.logs(&name).await,
    }
}

impl Proc {
    fn def<'a>(&'a self, name: &str) -> Result<&'a ProcessDef> {
        self.procs.get(name).ok_or_else(|| {
            let avail: Vec<_> = self.procs.keys().cloned().collect();
            anyhow::anyhow!("no process named {name:?}; defined: {avail:?}")
        })
    }

    /// Record a process lifecycle event to the latest session's processes.jsonl.
    fn log_event(&self, name: &str, action: &str) {
        if let Some(dir) = crate::session::latest_session_dir(&self.root) {
            crate::session::append_jsonl(
                &dir.join("processes.jsonl"),
                &ProcessRecord {
                    ts_ms: now_ms(),
                    name,
                    action,
                },
            );
        }
    }

    fn pid_file(&self, name: &str) -> String {
        format!("{}/{name}.pid", self.proc_dir)
    }
    fn log_file(&self, name: &str) -> String {
        format!("{}/{name}.log", self.proc_dir)
    }

    /// True if the process's recorded pid is alive in the container.
    async fn is_running(&self, name: &str) -> bool {
        let pid_file = self.pid_file(name);
        // Guard the empty-pid case ([ -n "$p" ]) so a missing pid file isn't a
        // false positive.
        let cmd = format!(
            "p=$(cat {pid_file} 2>/dev/null); [ -n \"$p\" ] && kill -0 \"$p\" 2>/dev/null && echo up"
        );
        match self.runtime.run_capture(&cmd, None, CONTROL_TIMEOUT).await {
            Ok((_, out)) => out.contains("up"),
            Err(_) => false,
        }
    }

    async fn list(&self) -> Result<()> {
        if self.procs.is_empty() {
            println!("no processes defined in .cowboy/agent.yaml");
            return Ok(());
        }
        #[allow(clippy::print_literal)]
        {
            println!("{:<16} {:<8} {}", "NAME", "STATUS", "COMMAND");
        }
        for (name, def) in &self.procs {
            let status = if self.is_running(name).await {
                "running"
            } else {
                "stopped"
            };
            println!("{name:<16} {status:<8} {}", def.command);
        }
        Ok(())
    }

    async fn start(&self, name: &str) -> Result<()> {
        let def = self.def(name)?.clone();
        if self.is_running(name).await {
            println!("{name} is already running");
            return Ok(());
        }
        let log = self.log_file(name);
        let pid = self.pid_file(name);
        // Create the proc dir, then launch a detached process group whose
        // leader pid we record for later signaling.
        let script = format!(
            "mkdir -p {dir}; cd {cwd}; setsid sh -c {cmd} > {log} 2>&1 & echo $! > {pid}",
            dir = self.proc_dir,
            cwd = shell_quote(&def.cwd),
            cmd = shell_quote(&def.command),
            log = log,
            pid = pid,
        );
        let (res, out) = self
            .runtime
            .run_capture(&script, None, CONTROL_TIMEOUT)
            .await?;
        if res.exit_code != 0 {
            bail!("failed to start {name}: {}", out.trim());
        }
        self.log_event(name, "start");
        println!("started {name}: {}", def.command);
        Ok(())
    }

    async fn stop(&self, name: &str) -> Result<()> {
        self.def(name)?;
        let pid = self.pid_file(name);
        // SIGTERM the whole group, wait briefly, then SIGKILL.
        let script = format!(
            "p=$(cat {pid} 2>/dev/null); [ -n \"$p\" ] || {{ echo 'not running'; exit 0; }}; \
             kill -TERM -\"$p\" 2>/dev/null; sleep 2; kill -KILL -\"$p\" 2>/dev/null; \
             rm -f {pid}; echo stopped",
            pid = pid
        );
        let (_, out) = self
            .runtime
            .run_capture(&script, None, CONTROL_TIMEOUT)
            .await?;
        self.log_event(name, "stop");
        println!("{name}: {}", out.trim());
        Ok(())
    }

    async fn restart(&self, name: &str) -> Result<()> {
        self.stop(name).await?;
        self.start(name).await
    }

    async fn logs(&self, name: &str) -> Result<()> {
        self.def(name)?;
        let log = self.log_file(name);
        // Follow the log (inherits the terminal; Ctrl-C to stop).
        let argv = vec![
            "sh".to_string(),
            "-lc".to_string(),
            format!("tail -n 200 -f {log}"),
        ];
        let _ = self
            .runtime
            .run(&argv)
            .await
            .with_context(|| format!("tailing logs for {name}"))?;
        let _ = &self.workdir; // workdir retained for future use
        Ok(())
    }
}

/// Quote a string as a single POSIX shell word.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("npm run dev"), "'npm run dev'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}
