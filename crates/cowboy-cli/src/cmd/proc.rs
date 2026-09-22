//! `cowboy proc ...` — supervise long-running processes defined in `agent.yaml`.
//!
//! Processes run inside the sandbox as detached process groups; state (pid + logs)
//! lives under `/workspace/.cowboy/proc/`.
//!
//! A process keeps the filesystem view it started with: a Landlock domain is fixed
//! at `exec` and can only narrow, so a path granted afterwards is invisible to it
//! until it is restarted. `cowboy proc list` reports that rather than leaving it to
//! be debugged.

use anyhow::{bail, Context, Result};
use cowboy_core::config::{AgentConfig, ConfigPaths, ProcessDef, SecurityConfig};

use crate::cli::{ProcArgs, ProcCommand};
use crate::sandbox::Sandbox;

const CONTROL_TIMEOUT: u64 = 30;

struct Proc {
    runtime: Box<dyn Sandbox>,
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
    let agent_cfg = AgentConfig::load_opt(&paths.agent)
        .with_context(|| format!("loading {}", paths.agent.display()))?
        .unwrap_or_default();
    let workdir = security.sandbox.workdir.clone();
    let proc_dir = format!("{workdir}/.cowboy/proc");
    // No UI here, so an `ask` has nobody to answer it: fail closed explicitly.
    let runtime = crate::cmd::sandbox::open_with(
        root.clone(),
        security,
        std::sync::Arc::new(cowboy_gateway::DenyAll),
    )?;

    let ctx = Proc {
        runtime: Box::new(runtime),
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
        // This used to run `setsid sh -c '<cmd>' &` inside a one-off sandbox and
        // print "started". It never worked: bwrap is PID 1 of that command's own PID
        // namespace, so when the command returned the kernel reaped everything in the
        // namespace — `setsid` included. The process was dead before this printed, and
        // `list` would show it "stopped" a second later.
        //
        // A background process has to be owned by something that outlives a single
        // command, and a CLI invocation is not that. The session's worker is: it holds
        // the sandbox for the whole session, so `--die-with-parent` gives the process
        // the session's lifetime and teardown reaps it. That is where starting one
        // lives now — the agent's `proc` tool — and there is no honest way to do it
        // from here.
        bail!(
            "`cowboy proc start` cannot start {name}: a background process is owned by the \
             session that starts it (it has to be, or nothing reaps it), and this command \
             exits immediately.\n\
             \n\
             - In a session, ask the agent to start it — it has a `proc` tool, and \
               {name} is already defined in agent.yaml: `{}`\n\
             - To run it yourself, use `cowboy shell` and start it there.\n\
             \n\
             `cowboy proc list` and `cowboy proc logs {name}` still work.",
            def.command
        )
    }

    async fn stop(&self, name: &str) -> Result<()> {
        self.def(name)?;
        let pid = self.pid_file(name);
        // Kept for the stale pid files an older cowboy may have left behind: SIGTERM
        // the whole group, wait briefly, then SIGKILL. A process started by the current
        // code is the worker's child and ends with the session, so there is normally
        // nothing here to stop.
        let script = format!(
            "p=$(cat {pid} 2>/dev/null); [ -n \"$p\" ] || {{ echo 'not running (processes \
             started in a session end with it)'; exit 0; }}; \
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
