//! [`NativeSandbox`]: the host-native implementation of the [`Sandbox`] seam.
//!
//! Owns the session namespaces, the current grant set, and any background
//! processes, and builds a fresh [`SandboxPlan`] for every command. That
//! per-command rebuild is what makes runtime grants work: a path approved a moment
//! ago is simply an entry in the next plan. Docker could not do this, because a
//! container's mounts are fixed when it is created.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use cowboy_core::config::{ProcessDef, SecurityConfig};
use cowboy_gateway::state::GatewayState;
use cowboy_sandbox::plan::{Grant, PlanInputs, SandboxPlan};
use cowboy_sandbox::{Denylist, HostProbe};

use super::bwrap::NetMode;
use super::exec::{self, ExecRequest};
use super::grants;
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
    /// Paths whose persisted grant was refused and already reported, so the notice
    /// is not repeated for every command in a session.
    reported_denials: Mutex<BTreeSet<PathBuf>>,
    /// Where persisted grants are read from and written to. Held as a field rather
    /// than looked up per call so tests can point it at a temp directory instead of
    /// the developer's real config.
    grants_dir: PathBuf,
    status: Mutex<Option<StatusTx>>,
    probe: Box<dyn HostProbe + Send + Sync>,
    session_name: String,
    /// The policy engine that answers the relay.
    ///
    /// Constructed up front rather than attached later: a sandbox with no engine
    /// would leave every connection blocked waiting for a verdict that never comes,
    /// so there is no useful half-configured state to represent.
    policy_engine: Arc<GatewayState>,
}

impl NativeSandbox {
    /// Build a sandbox.
    ///
    /// `approver` decides `ask` verdicts. Pass `cowboy_gateway::DenyAll` for a
    /// non-interactive caller — explicitly, so that failing closed is a choice made
    /// at the call site rather than a consequence of forgetting to wire a UI.
    pub fn new(
        root: PathBuf,
        security: SecurityConfig,
        probe: Box<dyn HostProbe + Send + Sync>,
        approver: Arc<dyn cowboy_gateway::Approver>,
    ) -> Result<Self> {
        let mask_file = crate::project::ensure_mask_file()?;
        let session_name = crate::project::session_name_for(&root);
        // Persisted project/global approvals are merged in here, in one place, so the
        // policy the engine enforces is the same one `cowboy sandbox plan` describes.
        let mut policy = security.network_policy.clone();
        crate::net::approvals::merge_into(&mut policy, &crate::net::approvals::load(&root));
        let policy_engine = Arc::new(GatewayState::new(
            policy,
            cowboy_gateway::dns::DnsMap::new(),
            approver,
        ));
        Ok(Self {
            policy_engine,
            root,
            security,
            mask_file,
            session: tokio::sync::Mutex::new(None),
            grants: Mutex::new(Vec::new()),
            grant_generation: AtomicU64::new(0),
            processes: Mutex::new(BTreeMap::new()),
            reported_denials: Mutex::new(BTreeSet::new()),
            grants_dir: grants::dir(),
            status: Mutex::new(None),
            probe,
            session_name,
        })
    }

    /// Read and write persisted grants under `dir` instead of the host config dir.
    ///
    /// Production callers use the default. This exists so tests can point at a temp
    /// directory: one that read the developer's real `global.json` would behave
    /// differently on every machine, and one that wrote there would leave grants
    /// behind on a real project.
    pub fn with_grants_dir(mut self, dir: PathBuf) -> Self {
        self.grants_dir = dir;
        self
    }

    /// The network policy in force, for the caller to log. The sandbox owns the
    /// merge of configured policy and persisted approvals, so this is the one
    /// authority on what is being enforced.
    pub fn policy(&self) -> &cowboy_core::config::NetworkPolicy {
        self.policy_engine.policy()
    }

    /// Build the plan for the *next* command, from the current grant set.
    ///
    /// The set is assembled here, per command, from two sources: grants approved in
    /// this session, and grants persisted host-side for this project or globally.
    /// Reading the persisted file every time is what lets `cowboy grant` in another
    /// terminal affect a session that is already running — the next command simply
    /// has a longer bind list — with no IPC to the worker at all.
    ///
    /// **Every grant is denylist-checked here**, whatever its origin. Checking only
    /// at approval time would not be enough: a global grant outlives the project it
    /// was made in, and both files are hand-editable. A denied entry is dropped and
    /// reported rather than failing the command, so a stale entry cannot wedge a
    /// session — but it is reported *once* per path, because this runs per command.
    pub fn plan(&self) -> Result<SandboxPlan> {
        let grants = self.effective_grants();
        let inputs = PlanInputs {
            root: &self.root,
            security: &self.security,
            grants: &grants,
            mask_file: &self.mask_file,
            relay_port: super::RELAY_PORT,
        };
        SandboxPlan::build(&inputs, self.probe.as_ref()).map_err(anyhow::Error::new)
    }

    /// Session grants first, then persisted ones, minus anything the denylist
    /// refuses. Session grants lead so an in-session decision wins over a stale
    /// persisted entry for the same path.
    fn effective_grants(&self) -> Vec<Grant> {
        let denylist = Denylist::build(self.probe.as_ref(), &self.root);
        let session = self.grants.lock().expect("grants poisoned").clone();
        let mut out: Vec<Grant> = Vec::with_capacity(session.len());
        for grant in session
            .into_iter()
            .chain(grants::load_in(&self.grants_dir, &self.root))
        {
            if out.iter().any(|g: &Grant| g.path == grant.path) {
                continue;
            }
            if let Some(reason) = denylist.check(&grant.path) {
                self.report_denied_grant(&grant, &reason);
                continue;
            }
            out.push(grant);
        }
        out
    }

    /// Report a refused grant once. `plan()` runs per command, so an unconditional
    /// message would repeat on every step of a task.
    fn report_denied_grant(&self, grant: &Grant, reason: &cowboy_sandbox::DenyReason) {
        let first = self
            .reported_denials
            .lock()
            .expect("reported denials poisoned")
            .insert(grant.path.clone());
        if first {
            self.report(format!(
                "ignoring the saved grant for {}: {}. Remove it with `cowboy grant --remove {}`.",
                grant.path.display(),
                reason.explain(),
                grant.path.display()
            ));
        }
        tracing::warn!(path = %grant.path.display(), "refusing a persisted grant");
    }

    /// Approve a path for subsequent commands, remembering it for `persistence`.
    ///
    /// Re-checked against the denylist here as well as in [`Self::plan`]: this is the
    /// call an interactive approval comes through, and refusing at the point of
    /// approval gives the user an error they can act on rather than a grant that
    /// silently does nothing.
    pub fn add_grant(
        &self,
        path: PathBuf,
        read_only: bool,
        persistence: grants::Persistence,
    ) -> Result<()> {
        let denylist = Denylist::build(self.probe.as_ref(), &self.root);
        if let Some(reason) = denylist.check(&path) {
            anyhow::bail!("{}", reason.explain());
        }
        let grant = Grant { path, read_only };
        if persistence != grants::Persistence::Session {
            grants::add_in(&self.grants_dir, &self.root, &grant, persistence)
                .with_context(|| format!("saving the grant for {}", grant.path.display()))?;
        }
        {
            let mut held = self.grants.lock().expect("grants poisoned");
            match held.iter_mut().find(|g| g.path == grant.path) {
                // Already granted with the same access: nothing changes, and in
                // particular no running process needs warning about.
                Some(existing) if existing.read_only == grant.read_only => return Ok(()),
                Some(existing) => existing.read_only = grant.read_only,
                None => held.push(grant),
            }
        }
        self.grant_generation.fetch_add(1, Ordering::SeqCst);
        self.warn_about_stale_processes();
        Ok(())
    }

    /// The grants in force for the next command, including persisted ones.
    pub fn grants(&self) -> Vec<Grant> {
        self.effective_grants()
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
            // The plan's limits, so what `cowboy sandbox plan` prints is what the
            // session is actually held to.
            let limits = self.plan()?.limits;
            let (session, channels) = SessionSandbox::start(&self.session_name, &exe, &limits)?;
            match session.limits_in_force() {
                Some(s) => self.report(format!("resource limits: {s}")),
                None if limits.memory_mib.is_some() || limits.cpus.is_some() => self.report(
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
            let engine = self.policy_engine.clone();
            let connect = channels.connect;
            std::thread::Builder::new()
                .name("cowboy-egress-broker".into())
                .spawn({
                    let handle = handle.clone();
                    move || {
                        crate::sandbox::transport::broker::serve_blocking(connect, engine, handle);
                    }
                })
                .context("spawning the egress policy broker")?;

            // The upstream resolver is read here, on the host, and the sandbox is
            // never told which one it is: it sends every query to a loopback port and
            // the answer comes back from this side.
            let upstream = cowboy_gateway::dns::host_resolver();
            let engine = self.policy_engine.clone();
            let resolve = channels.resolve;
            std::thread::Builder::new()
                .name("cowboy-dns-broker".into())
                .spawn(move || {
                    crate::sandbox::transport::broker::serve_dns_blocking(
                        resolve, engine, handle, upstream,
                    );
                })
                .context("spawning the dns policy broker")?;
            *guard = Some(session);
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
    fn add_grant(
        &self,
        path: &Path,
        read_only: bool,
        persistence: grants::Persistence,
    ) -> Result<()> {
        // The inherent method takes an owned path; forward to it so there is one
        // implementation of the denylist check and the staleness warning.
        NativeSandbox::add_grant(self, path.to_path_buf(), read_only, persistence)
    }

    fn granted_paths(&self) -> Vec<(PathBuf, bool)> {
        self.effective_grants()
            .into_iter()
            .map(|g| (g.path, g.read_only))
            .collect()
    }

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

    /// A sandbox whose grant store is a fresh temp directory.
    ///
    /// The `TempDir` is returned and must be kept alive: it is the guard that stops
    /// the test reading (or writing) the developer's real `~/.config/cowboy/grants`,
    /// where a stray `global.json` would change what every plan here contains.
    fn sandbox_with_store(root: &Path) -> (NativeSandbox, assert_fs::TempDir) {
        let store = assert_fs::TempDir::new().unwrap();
        let probe = FakeHost::new().with_existing(["/usr", root.to_str().unwrap()]);
        let s = NativeSandbox::new(
            root.to_path_buf(),
            SecurityConfig::default(),
            Box::new(probe),
            Arc::new(cowboy_gateway::DenyAll),
        )
        .unwrap()
        .with_grants_dir(store.path().to_path_buf());
        (s, store)
    }

    fn sandbox(root: &Path) -> NativeSandbox {
        sandbox_with_store(root).0
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

        s.add_grant(
            PathBuf::from("/srv/other"),
            true,
            grants::Persistence::Session,
        )
        .unwrap();

        let after = s.plan().unwrap();
        let b = after
            .binds
            .iter()
            .find(|b| b.source == Path::new("/srv/other"))
            .expect("the grant should be in the next plan");
        assert_eq!(b.mode, cowboy_sandbox::BindMode::ReadOnly);
    }

    /// The denylist applies here too — a refusal at approval time is an error the
    /// user can act on.
    #[test]
    fn a_denylisted_grant_is_refused() {
        let s = sandbox(Path::new("/srv/proj"));
        let err = s
            .add_grant(
                PathBuf::from("/home/dev/.aws"),
                true,
                grants::Persistence::Session,
            )
            .expect_err("credentials must be refused");
        assert!(err.to_string().contains("cowboy secrets add"), "{err}");
        assert!(s.grants().is_empty());
    }

    /// The one that actually holds the line. A grant file is host-owned but
    /// hand-editable, and a *global* grant outlives the project it was made in — so
    /// checking only at approval time would let an entry naming credentials be
    /// honoured forever after. Every grant is re-checked when the plan is built.
    #[test]
    fn a_persisted_grant_naming_credentials_is_refused_when_used() {
        let (mut s, store) = sandbox_with_store(Path::new("/srv/proj"));
        let mut rx = s.status_channel();

        // Write it straight into the store, bypassing `add_grant` — exactly what a
        // hand-edited file (or one written by an older, laxer version) looks like.
        grants::add_in(
            store.path(),
            Path::new("/srv/proj"),
            &Grant {
                path: PathBuf::from("/home/dev/.aws"),
                read_only: true,
            },
            grants::Persistence::Global,
        )
        .unwrap();

        let plan = s.plan().unwrap();
        assert!(
            !plan
                .binds
                .iter()
                .any(|b| b.source == Path::new("/home/dev/.aws")),
            "a saved grant for a credential store must not reach the bind list: {:?}",
            plan.binds
        );
        let msg = rx.try_recv().expect("the refusal must be reported");
        assert!(
            msg.contains(".aws"),
            "the notice should name the path: {msg}"
        );
        assert!(
            msg.contains("--remove"),
            "the notice should say how to clear it: {msg}"
        );
    }

    /// The refusal is reported once, not on every command — `plan()` runs per command
    /// and a repeated notice would bury the session's real output.
    #[test]
    fn a_refused_grant_is_reported_only_once() {
        let (mut s, store) = sandbox_with_store(Path::new("/srv/proj"));
        let mut rx = s.status_channel();
        grants::add_in(
            store.path(),
            Path::new("/srv/proj"),
            &Grant {
                path: PathBuf::from("/home/dev/.aws"),
                read_only: true,
            },
            grants::Persistence::Project,
        )
        .unwrap();

        for _ in 0..3 {
            let _ = s.plan().unwrap();
        }
        assert!(rx.try_recv().is_ok(), "reported the first time");
        assert!(
            rx.try_recv().is_err(),
            "and not again for every later command"
        );
    }

    /// A grant saved for the project is in force for a new session without being
    /// re-approved — and, because the plan is rebuilt per command, it also reaches a
    /// session that was already running when `cowboy grant` wrote it.
    #[test]
    fn a_persisted_grant_is_in_force_without_being_re_approved() {
        let (s, store) = sandbox_with_store(Path::new("/srv/proj"));
        assert!(!s
            .plan()
            .unwrap()
            .binds
            .iter()
            .any(|b| b.source == Path::new("/srv/other")));

        // Stands in for `cowboy grant /srv/other` running in another terminal.
        grants::add_in(
            store.path(),
            Path::new("/srv/proj"),
            &Grant {
                path: PathBuf::from("/srv/other"),
                read_only: false,
            },
            grants::Persistence::Project,
        )
        .unwrap();

        let b = s
            .plan()
            .unwrap()
            .binds
            .into_iter()
            .find(|b| b.source == Path::new("/srv/other"))
            .expect("the saved grant should apply to the next command");
        assert_eq!(b.mode, cowboy_sandbox::BindMode::ReadWrite);
    }

    /// A session decision wins over a stale saved entry for the same path, so
    /// tightening access in-session is not undone by what is on disk.
    #[test]
    fn a_session_grant_wins_over_a_persisted_one_for_the_same_path() {
        let (s, store) = sandbox_with_store(Path::new("/srv/proj"));
        grants::add_in(
            store.path(),
            Path::new("/srv/proj"),
            &Grant {
                path: PathBuf::from("/srv/other"),
                read_only: false,
            },
            grants::Persistence::Project,
        )
        .unwrap();
        s.add_grant(
            PathBuf::from("/srv/other"),
            true,
            grants::Persistence::Session,
        )
        .unwrap();

        let matching: Vec<_> = s
            .plan()
            .unwrap()
            .binds
            .into_iter()
            .filter(|b| b.source == Path::new("/srv/other"))
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "one bind per path, not two: {matching:?}"
        );
        assert_eq!(matching[0].mode, cowboy_sandbox::BindMode::ReadOnly);
    }

    /// Approving at project scope writes it down; approving for the session does not.
    #[test]
    fn only_a_persistent_scope_is_written_to_the_store() {
        let (s, store) = sandbox_with_store(Path::new("/srv/proj"));
        s.add_grant(
            PathBuf::from("/srv/session-only"),
            true,
            grants::Persistence::Session,
        )
        .unwrap();
        assert!(grants::load_in(store.path(), Path::new("/srv/proj")).is_empty());

        s.add_grant(
            PathBuf::from("/srv/kept"),
            true,
            grants::Persistence::Project,
        )
        .unwrap();
        assert_eq!(
            grants::load_in(store.path(), Path::new("/srv/proj"))
                .into_iter()
                .map(|g| g.path)
                .collect::<Vec<_>>(),
            vec![PathBuf::from("/srv/kept")]
        );
    }

    #[test]
    fn granting_the_same_path_twice_is_idempotent() {
        let s = sandbox(Path::new("/srv/proj"));
        s.add_grant(
            PathBuf::from("/srv/other"),
            true,
            grants::Persistence::Session,
        )
        .unwrap();
        s.add_grant(
            PathBuf::from("/srv/other"),
            true,
            grants::Persistence::Session,
        )
        .unwrap();
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

        s.add_grant(
            PathBuf::from("/srv/other"),
            true,
            grants::Persistence::Session,
        )
        .unwrap();

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

        s.add_grant(
            PathBuf::from("/srv/other"),
            true,
            grants::Persistence::Session,
        )
        .unwrap();

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

        s.add_grant(
            PathBuf::from("/srv/other"),
            true,
            grants::Persistence::Session,
        )
        .unwrap();
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
