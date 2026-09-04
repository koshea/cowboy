//! `cowboy sandbox` — inspect the confinement boundary.
//!
//! The boundary should be inspectable without reading the source or trusting a
//! summary of it. `plan` renders exactly what the next command would get: which
//! paths are readable and writable, what is masked, what the kernel-level
//! lockdown is, and which paths can never be granted at runtime.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cowboy_core::config::{expand_path, ConfigPaths, SecurityConfig};
use cowboy_sandbox::plan::{PlanInputs, SandboxPlan};
use cowboy_sandbox::{Denylist, HostProbe};

use crate::cli::{SandboxArgs, SandboxCommand};
use crate::sandbox::Sandbox;

pub async fn run(args: SandboxArgs) -> Result<()> {
    match args.command {
        SandboxCommand::Plan => plan(),
        SandboxCommand::Exec { command } => exec(command).await,
    }
}

/// Open this project's sandbox.
///
/// One constructor for every entry point — the CLI, the worker, and a one-shot run —
/// so they cannot drift on which config is loaded or which host probe is used.
/// `approver` decides `ask` verdicts; pass `cowboy_gateway::DenyAll` where there is
/// no UI to ask, explicitly, so failing closed is a choice made at the call site.
pub(crate) fn open(
    root: std::path::PathBuf,
    approver: std::sync::Arc<dyn cowboy_gateway::Approver>,
) -> Result<crate::sandbox::native::NativeSandbox> {
    let security = load(&root)?;
    crate::sandbox::native::NativeSandbox::new(root, security, Box::new(RealHost), approver)
}

/// As [`open`], but with a security config the caller has already adjusted (the
/// worker gates credential grants interactively before the sandbox is built).
pub(crate) fn open_with(
    root: std::path::PathBuf,
    security: cowboy_core::config::SecurityConfig,
    approver: std::sync::Arc<dyn cowboy_gateway::Approver>,
) -> Result<crate::sandbox::native::NativeSandbox> {
    crate::sandbox::native::NativeSandbox::new(root, security, Box::new(RealHost), approver)
}

/// Load the effective security config for this project, personal overlay merged.
pub(crate) fn load(root: &Path) -> Result<cowboy_core::config::SecurityConfig> {
    let paths = ConfigPaths::for_root(root);
    // The `cowboy init` hint matters: a missing file is overwhelmingly a project that
    // has not been set up, and "config file not found" alone leaves the reader to
    // guess what creates it.
    let mut security = SecurityConfig::load(&paths.security).with_context(|| {
        format!(
            "loading {} (run `cowboy init` first)",
            paths.security.display()
        )
    })?;
    // Merge the personal overlay so this matches what a session would actually
    // get; without it the boundary shown here would be narrower than the real one.
    cowboy_core::usersecrets::merge_into(&mut security, &crate::project::repo_key(root));
    Ok(security)
}

/// Run one command in the sandbox.
///
/// Uses the full session: namespaces, egress interception, and the policy engine.
/// The approver is `DenyAll`, so an `ask` denies — this is a one-off CLI path with no
/// UI attached, and the alternative to saying so explicitly is a prompt nobody can
/// answer silently becoming an allow.
async fn exec(command: Vec<String>) -> Result<()> {
    let root = crate::cmd::project_root()?;
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let sandbox = open(root, std::sync::Arc::new(cowboy_gateway::DenyAll))?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    // Print as it arrives; a build should not look hung while it works.
    let printer = tokio::spawn(async move {
        use std::io::Write;
        while let Some(chunk) = rx.recv().await {
            print!("{chunk}");
            let _ = std::io::stdout().flush();
        }
    });

    let (result, _) = sandbox
        .exec_stream(
            &command.join(" "),
            None,
            0,
            tokio_util::sync::CancellationToken::new(),
            tx,
        )
        .await?;
    printer.await.ok();
    sandbox.stop().await;

    if result.exit_code != 0 {
        std::process::exit(result.exit_code);
    }
    Ok(())
}

/// The real host, as seen by the plan builder.
pub struct RealHost;

impl HostProbe for RealHost {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn git_common_dir(&self, root: &Path) -> Option<PathBuf> {
        crate::project::git_common_dir(root)
    }

    fn expand(&self, raw: &str) -> Option<PathBuf> {
        expand_path(raw).ok()
    }

    fn home(&self) -> Option<PathBuf> {
        // Reuse config's expansion rather than adding a home-dir dependency, so
        // `~` resolves identically here and in every configured path.
        expand_path("~").ok()
    }

    /// The running cowboy binary — the plan bind-mounts it as the lockdown shim.
    ///
    /// Goes through [`crate::project::self_exe`] rather than `current_exe()` directly,
    /// because `current_exe()` on a **replaced** binary (`cargo install` during a live
    /// session) yields `".../cowboy (deleted)"`. The shim bind is rendered
    /// `--ro-bind-try`, so that missing source is silently skipped and every command in
    /// the session then dies with `bwrap: execvp /.cowboy-shim: No such file or
    /// directory` — a session alive but unable to run anything.
    fn self_exe(&self) -> Option<PathBuf> {
        crate::project::self_exe().ok()
    }
}

fn plan() -> Result<()> {
    let root = crate::cmd::project_root()?;
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let security = load(&root)?;

    let probe = RealHost;
    let denylist = Denylist::build(&probe, &root);
    // Include saved grants, and drop any the denylist now refuses — exactly what
    // `NativeSandbox::plan` does per command. Rendering `&[]` here would make this
    // command describe a narrower boundary than the one commands actually run in,
    // which is the one thing it must never do.
    let grants: Vec<_> = crate::sandbox::grants::load_in(&crate::sandbox::grants::dir(), &root)
        .into_iter()
        .filter(|g| denylist.check(&g.path).is_none())
        .collect();
    // A representative mask path; the executor creates the real one per session.
    let mask = PathBuf::from("<mask: empty read-only file>");
    // Likewise representative. Printing the boundary must not create anything: the
    // real directory belongs to a running session, keyed to the process that owns it,
    // and inventing one here would leave litter behind for the reaper.
    let scratch = PathBuf::from("<session scratch>");
    let inputs = PlanInputs {
        root: &root,
        security: &security,
        grants: &grants,
        mask_file: &mask,
        relay_port: crate::sandbox::RELAY_PORT,
        scratch: &scratch,
    };
    let plan = SandboxPlan::build(&inputs, &probe)?;
    println!("project {}\n", root.display());
    print!("{}", plan.render(&denylist));
    // The plan describes what a command *gets*; a configured ceiling this host cannot
    // apply would otherwise be printed as though it were in force.
    let configured = plan.limits.memory_mib.is_some()
        || plan.limits.cpus.is_some()
        || plan.limits.pids.is_some();
    if configured && !crate::sandbox::cgroup::available() {
        println!(
            "  NOT ENFORCED: no delegated cgroup v2 subtree on this host. \
             Run `cowboy doctor` for what to change."
        );
    }
    Ok(())
}
