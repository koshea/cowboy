//! [`NativeSandbox`]: the host-native implementation of the [`Sandbox`] seam.
//!
//! Owns the session namespaces, the current grant set, and any background
//! processes, and builds a fresh [`SandboxPlan`] for every command. That
//! per-command rebuild is what makes runtime grants work: a path approved a moment
//! ago is simply an entry in the next plan. Docker could not do this, because a
//! container's mounts are fixed when it is created.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::{Context, Result};
use async_trait::async_trait;
use cowboy_core::config::{ProcessDef, SecurityConfig};
use cowboy_sandbox::plan::{Grant, PlanInputs, SandboxPlan};
use cowboy_sandbox::{Denylist, HostProbe};

use super::bwrap::NetMode;
use super::exec::{self, ExecRequest};
use super::session::SessionSandbox;
use super::{ExecResult, Sandbox, StatusRx, StatusTx};

/// A background process started from `agent.yaml`.
struct Background {
    /// The bwrap monitor pid. Killing it reaps the process's whole namespace.
    pid: u32,
    /// The grant generation current when it started.
    ///
    /// A Landlock domain is fixed at `exec` and can only ever be narrowed, so a
    /// grant approved afterwards is invisible to this process however long it runs.
    /// Comparing generations is how we can tell the user that, instead of leaving
    /// them to debug a dev server that cannot see a folder they just approved.
    grant_generation: u64,
    child: Option<tokio::process::Child>,
}

pub struct NativeSandbox {
    root: PathBuf,
    security: SecurityConfig,
    mask_file: PathBuf,
    /// Started lazily: constructing a sandbox must not create namespaces, so that
    /// `cowboy sandbox plan` and the unit tests stay side-effect free.
    session: tokio::sync::Mutex<Option<SessionSandbox>>,
    grants: Mutex<Vec<Grant>>,
    /// Bumped on every grant. Background processes record the value they started
    /// with; a mismatch means their Landlock domain predates a grant.
    grant_generation: AtomicU64,
    processes: Mutex<BTreeMap<String, Background>>,
    status: Mutex<Option<StatusTx>>,
    probe: Box<dyn HostProbe + Send + Sync>,
    session_name: String,
}

impl NativeSandbox {
    pub fn new(
        root: PathBuf,
        security: SecurityConfig,
        probe: Box<dyn HostProbe + Send + Sync>,
    ) -> Result<Self> {
        let mask_file = crate::net::runtime::ensure_mask_file()?;
        let session_name = format!("cowboy-{:08x}", crate::net::runtime::project_hash(&root));
        Ok(Self {
            root,
            security,
            mask_file,
            session: tokio::sync::Mutex::new(None),
            grants: Mutex::new(Vec::new()),
            grant_generation: AtomicU64::new(0),
            processes: Mutex::new(BTreeMap::new()),
            status: Mutex::new(None),
            probe,
            session_name,
        })
    }

    /// Build the plan for the *next* command, from the current grant set.
    pub fn plan(&self) -> Result<SandboxPlan> {
        let grants = self.grants.lock().expect("grants poisoned").clone();
        let inputs = PlanInputs {
            root: &self.root,
            security: &self.security,
            grants: &grants,
            mask_file: &self.mask_file,
            relay_port: super::RELAY_PORT,
        };
        SandboxPlan::build(&inputs, self.probe.as_ref()).map_err(anyhow::Error::new)
    }

    /// Approve a path for subsequent commands.
    ///
    /// Re-checked against the denylist here as well as at approval time: this is the
    /// call a persisted or hand-edited grant also passes through, so it is the one
    /// that has to hold.
    pub fn add_grant(&self, path: PathBuf, read_only: bool) -> Result<()> {
        let denylist = Denylist::build(self.probe.as_ref(), &self.root);
        if let Some(reason) = denylist.check(&path) {
            anyhow::bail!("{}", reason.explain());
        }
        {
            let mut grants = self.grants.lock().expect("grants poisoned");
            if grants.iter().any(|g| g.path == path) {
                return Ok(());
            }
            grants.push(Grant { path, read_only });
        }
        self.grant_generation.fetch_add(1, Ordering::SeqCst);
        self.warn_about_stale_processes();
        Ok(())
    }

    pub fn grants(&self) -> Vec<Grant> {
        self.grants.lock().expect("grants poisoned").clone()
    }

    /// Running processes whose Landlock domain predates the current grant set, and
    /// so cannot see every granted path.
    ///
    /// Reported at grant time, and available afterwards so `cowboy proc list` can
    /// keep showing it — the staleness persists until the process is restarted, not
    /// just for the moment the grant was approved.
    pub fn stale_processes(&self) -> Vec<String> {
        let current = self.grant_generation.load(Ordering::SeqCst);
        let procs = self.processes.lock().expect("processes poisoned");
        procs
            .iter()
            .filter(|(_, b)| b.grant_generation < current)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Tell the user which running processes cannot see a new grant.
    ///
    /// This is *every* process running at the time: a Landlock domain is fixed when
    /// a process starts and can only ever be narrowed, so anything already running
    /// necessarily predates the grant. The behaviour is correct but invisible and
    /// confusing to hit — a dev server keeps failing to read a folder that every new
    /// command reads fine — so it is said out loud rather than left to be debugged.
    fn warn_about_stale_processes(&self) {
        let stale: Vec<String> = self
            .processes
            .lock()
            .expect("processes poisoned")
            .keys()
            .cloned()
            .collect();
        if stale.is_empty() {
            return;
        }
        let names = stale.join(", ");
        self.report(format!(
            "the new grant does not apply to already-running process{} {names}: a sandbox's \
             filesystem permissions are fixed when it starts and can never be widened. \
             Restart {} with `cowboy proc restart` to pick it up.",
            if stale.len() == 1 { "" } else { "es" },
            if stale.len() == 1 { "it" } else { "them" },
        ));
    }

    fn report(&self, msg: String) {
        if let Some(tx) = self.status.lock().expect("status poisoned").as_ref() {
            let _ = tx.send(msg);
        }
    }

    /// The session, started on first use.
    ///
    /// Fails closed: if the namespaces cannot be created, no command runs. There is
    /// no fallback to running on the host.
    async fn session(&self) -> Result<tokio::sync::MutexGuard<'_, Option<SessionSandbox>>> {
        let mut guard = self.session.lock().await;
        // A holder that died (OOM, external kill) must not be silently reused: its
        // namespace paths are gone, so commands would fail confusingly.
        if guard.as_ref().is_some_and(|s| !s.is_alive()) {
            tracing::warn!("the sandbox session holder died; starting a new session");
            *guard = None;
        }
        if guard.is_none() {
            self.report("starting the sandbox session…".to_string());
            // The same binary the plan binds as the lockdown shim, so the two cannot
            // disagree about which cowboy is running.
            let exe = self
                .probe
                .self_exe()
                .context("cannot locate the cowboy binary to hold the session namespaces")?;
            *guard = Some(SessionSandbox::start(&self.session_name, &exe)?);
        }
        Ok(guard)
    }

    /// Run a command in this session, streaming output.
    async fn exec_in_session(
        &self,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: u64,
        cancel: tokio_util::sync::CancellationToken,
        chunks: StatusTx,
    ) -> Result<(ExecResult, String)> {
        let plan = self.plan()?;
        let guard = self.session().await?;
        let session = guard.as_ref().expect("session started");
        let req = ExecRequest {
            plan: &plan,
            command,
            cwd,
            timeout_secs,
            // Inherit: the session namespace is already entered, and unsharing here
            // would discard the network namespace the transport is installed in.
            net: NetMode::Inherit,
            session: Some(session),
        };
        exec::run_streaming(req, cancel, chunks).await
    }

    /// Start a background process from `agent.yaml`.
    ///
    /// It shares the session's network namespace, so later commands can reach it on
    /// loopback, but gets its own PID namespace so stopping it reaps exactly its own
    /// processes.
    pub async fn start_process(&self, name: &str, def: &ProcessDef) -> Result<()> {
        if self.process_is_running(name) {
            anyhow::bail!("process {name} is already running");
        }
        let plan = self.plan()?;
        let guard = self.session().await?;
        let session = guard.as_ref().expect("session started");
        let generation = self.grant_generation.load(Ordering::SeqCst);

        let child = exec::spawn_detached(
            &plan,
            &def.command,
            Some(&def.cwd),
            NetMode::Inherit,
            Some(session),
        )
        .await?;
        let pid = child.id().context("background process has no pid")?;
        self.processes.lock().expect("processes poisoned").insert(
            name.to_string(),
            Background {
                pid,
                grant_generation: generation,
                child: Some(child),
            },
        );
        Ok(())
    }

    pub fn process_is_running(&self, name: &str) -> bool {
        self.processes
            .lock()
            .expect("processes poisoned")
            .get(name)
            .is_some_and(|b| Path::new(&format!("/proc/{}", b.pid)).exists())
    }

    pub fn running_processes(&self) -> Vec<String> {
        self.processes
            .lock()
            .expect("processes poisoned")
            .keys()
            .cloned()
            .collect()
    }

    /// Stop a background process. Killing bwrap reaps its whole PID namespace, so
    /// nothing it spawned survives.
    pub async fn stop_process(&self, name: &str) -> Result<()> {
        let entry = self
            .processes
            .lock()
            .expect("processes poisoned")
            .remove(name);
        let Some(mut b) = entry else {
            anyhow::bail!("process {name} is not running");
        };
        if let Some(child) = b.child.as_mut() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        Ok(())
    }
}

#[async_trait]
impl Sandbox for NativeSandbox {
    fn root(&self) -> &Path {
        &self.root
    }

    fn session_name(&self) -> &str {
        &self.session_name
    }

    fn status_channel(&mut self) -> StatusRx {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *self.status.lock().expect("status poisoned") = Some(tx);
        rx
    }

    fn has_mise_config(&self) -> bool {
        const CONFIGS: &[&str] = &[
            "mise.toml",
            ".mise.toml",
            "mise/config.toml",
            ".mise/config.toml",
            ".config/mise/config.toml",
            ".tool-versions",
        ];
        CONFIGS.iter().any(|f| self.root.join(f).exists())
    }

    async fn ensure_running(&self) -> Result<()> {
        // Starting the session is the whole of bring-up; there is no image to pull
        // and no container to create.
        self.session().await.map(|_| ())
    }

    async fn stop(&self) {
        let names = self.running_processes();
        for name in names {
            let _ = self.stop_process(&name).await;
        }
        if let Some(mut s) = self.session.lock().await.take() {
            s.stop();
        }
    }

    async fn exec_stream(
        &self,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: u64,
        cancel: tokio_util::sync::CancellationToken,
        chunks: StatusTx,
    ) -> Result<(ExecResult, String)> {
        self.exec_in_session(command, cwd, timeout_secs, cancel, chunks)
            .await
    }

    async fn run_capture(
        &self,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: u64,
    ) -> Result<(ExecResult, String)> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        self.exec_in_session(
            command,
            cwd,
            timeout_secs,
            tokio_util::sync::CancellationToken::new(),
            tx,
        )
        .await
    }

    async fn run(&self, argv: &[String]) -> Result<ExecResult> {
        // Joined with spaces so ordinary shell syntax works, matching how the agent
        // and the Docker path both behaved.
        let (res, out) = self
            .run_capture(&argv.join(" "), None, 0)
            .await
            .context("running a command in the sandbox")?;
        print!("{out}");
        Ok(res)
    }

    async fn shell(&self) -> Result<ExecResult> {
        let plan = self.plan()?;
        let guard = self.session().await?;
        let session = guard.as_ref().expect("session started");
        exec::run_interactive(&plan, "bash -l", NetMode::Inherit, Some(session)).await
    }

    async fn fileop(&self, payload: &str) -> Result<(ExecResult, String)> {
        // The structured file tools run through the in-sandbox cowboy binary, which
        // is already bound read-only for the lockdown shim.
        let command = format!("{} x-fileop", cowboy_sandbox::SHIM_PATH);
        let plan = self.plan()?;
        let guard = self.session().await?;
        let session = guard.as_ref().expect("session started");
        exec::run_with_stdin(&plan, &command, payload, NetMode::Inherit, Some(session)).await
    }

    async fn stop_all_processes(&self) -> Result<()> {
        for name in self.running_processes() {
            let _ = self.stop_process(&name).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_sandbox::probe::FakeHost;

    fn sandbox(root: &Path) -> NativeSandbox {
        let probe = FakeHost::new().with_existing(["/usr", root.to_str().unwrap()]);
        NativeSandbox::new(
            root.to_path_buf(),
            SecurityConfig::default(),
            Box::new(probe),
        )
        .unwrap()
    }

    /// Construction must not create namespaces, or `cowboy sandbox plan` and the
    /// unit tests would need a working sandbox just to build a value.
    #[test]
    fn construction_is_side_effect_free() {
        let s = sandbox(Path::new("/srv/proj"));
        assert!(s.plan().is_ok(), "a plan can be built without a session");
    }

    /// The point of the per-command rebuild.
    #[test]
    fn a_grant_appears_in_the_next_plan() {
        let s = sandbox(Path::new("/srv/proj"));
        let before = s.plan().unwrap();
        assert!(!before
            .binds
            .iter()
            .any(|b| b.source == Path::new("/srv/other")));

        s.add_grant(PathBuf::from("/srv/other"), true).unwrap();

        let after = s.plan().unwrap();
        let b = after
            .binds
            .iter()
            .find(|b| b.source == Path::new("/srv/other"))
            .expect("the grant should be in the next plan");
        assert_eq!(b.mode, cowboy_sandbox::BindMode::ReadOnly);
    }

    /// The denylist applies here too — this is the path a persisted grant takes.
    #[test]
    fn a_denylisted_grant_is_refused() {
        let s = sandbox(Path::new("/srv/proj"));
        let err = s
            .add_grant(PathBuf::from("/home/dev/.aws"), true)
            .expect_err("credentials must be refused");
        assert!(err.to_string().contains("cowboy secrets add"), "{err}");
        assert!(s.grants().is_empty());
    }

    #[test]
    fn granting_the_same_path_twice_is_idempotent() {
        let s = sandbox(Path::new("/srv/proj"));
        s.add_grant(PathBuf::from("/srv/other"), true).unwrap();
        s.add_grant(PathBuf::from("/srv/other"), true).unwrap();
        assert_eq!(s.grants().len(), 1);
    }

    /// A running process cannot see a later grant, and the user must be told rather
    /// than left to debug it.
    #[test]
    fn a_grant_warns_about_already_running_processes() {
        let mut s = sandbox(Path::new("/srv/proj"));
        let mut rx = s.status_channel();
        // Simulate a process started at generation 0 without spawning anything.
        s.processes.lock().unwrap().insert(
            "web".to_string(),
            Background {
                pid: 1,
                grant_generation: 0,
                child: None,
            },
        );

        s.add_grant(PathBuf::from("/srv/other"), true).unwrap();

        let msg = rx.try_recv().expect("a notice should have been emitted");
        assert!(
            msg.contains("web"),
            "the notice should name the process: {msg}"
        );
        assert!(
            msg.contains("Restart"),
            "the notice should say what to do: {msg}"
        );
    }

    /// Every process running when a grant is added is named, because a Landlock
    /// domain is fixed at start and can never be widened — there is no such thing as
    /// a running process that already has a brand-new grant.
    #[test]
    fn every_running_process_is_named_when_a_grant_is_added() {
        let mut s = sandbox(Path::new("/srv/proj"));
        let mut rx = s.status_channel();
        for (name, pid) in [("web", 1u32), ("worker", 2)] {
            s.processes.lock().unwrap().insert(
                name.to_string(),
                Background {
                    pid,
                    grant_generation: 0,
                    child: None,
                },
            );
        }

        s.add_grant(PathBuf::from("/srv/other"), true).unwrap();

        let msg = rx.try_recv().expect("a notice should have been emitted");
        assert!(msg.contains("web") && msg.contains("worker"), "{msg}");
        assert!(msg.contains("processes"), "plural wording: {msg}");
    }

    /// Staleness must persist after the moment of approval, so `cowboy proc list`
    /// can keep reporting it until the process is restarted.
    #[test]
    fn staleness_persists_until_the_process_restarts() {
        let s = sandbox(Path::new("/srv/proj"));
        s.processes.lock().unwrap().insert(
            "web".to_string(),
            Background {
                pid: 1,
                grant_generation: 0,
                child: None,
            },
        );
        assert!(
            s.stale_processes().is_empty(),
            "no grants yet, nothing stale"
        );

        s.add_grant(PathBuf::from("/srv/other"), true).unwrap();
        assert_eq!(s.stale_processes(), vec!["web".to_string()]);

        // Restarting it clears the staleness.
        let current = s.grant_generation.load(Ordering::SeqCst);
        s.processes.lock().unwrap().insert(
            "web".to_string(),
            Background {
                pid: 2,
                grant_generation: current,
                child: None,
            },
        );
        assert!(
            s.stale_processes().is_empty(),
            "a restarted process is current"
        );
    }
}
