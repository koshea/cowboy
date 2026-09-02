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

pub async fn run(args: SandboxArgs) -> Result<()> {
    match args.command {
        SandboxCommand::Plan => plan(),
        SandboxCommand::Exec { command } => exec(command).await,
    }
}

/// Load the effective security config for this project, personal overlay merged.
fn load(root: &Path) -> Result<cowboy_core::config::SecurityConfig> {
    let paths = ConfigPaths::for_root(root);
    let mut security = SecurityConfig::load(&paths.security)
        .with_context(|| format!("loading {}", paths.security.display()))?;
    // Merge the personal overlay so this matches what a session would actually
    // get; without it the boundary shown here would be narrower than the real one.
    cowboy_core::usersecrets::merge_into(&mut security, &crate::net::runtime::repo_key(root));
    Ok(security)
}

/// Run one command in the sandbox with no network access.
///
/// Deliberately `NetMode::Isolated`: this path exists to inspect and test
/// confinement, and until the egress transport is installed a sandbox with an
/// inherited namespace would have the host's connectivity.
async fn exec(command: Vec<String>) -> Result<()> {
    let root = crate::cmd::project_root()?;
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let security = load(&root)?;
    let mask = crate::net::runtime::ensure_mask_file()?;
    let probe = RealHost;
    let plan = crate::sandbox::exec::plan_for(&root, &security, &mask, &probe)?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    // Print as it arrives; a build should not look hung while it works.
    let printer = tokio::spawn(async move {
        use std::io::Write;
        while let Some(chunk) = rx.recv().await {
            print!("{chunk}");
            let _ = std::io::stdout().flush();
        }
    });

    let request = crate::sandbox::exec::ExecRequest {
        plan: &plan,
        command: &command.join(" "),
        cwd: None,
        timeout_secs: 0,
        net: crate::sandbox::bwrap::NetMode::Isolated,
        session: None,
    };
    let (result, _) = crate::sandbox::exec::run_streaming(
        request,
        tokio_util::sync::CancellationToken::new(),
        tx,
    )
    .await?;
    printer.await.ok();

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
        crate::net::runtime::git_common_dir(root)
    }

    fn expand(&self, raw: &str) -> Option<PathBuf> {
        expand_path(raw).ok()
    }

    fn home(&self) -> Option<PathBuf> {
        // Reuse config's expansion rather than adding a home-dir dependency, so
        // `~` resolves identically here and in every configured path.
        expand_path("~").ok()
    }

    fn self_exe(&self) -> Option<PathBuf> {
        std::env::current_exe().ok()
    }
}

fn plan() -> Result<()> {
    let root = crate::cmd::project_root()?;
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let security = load(&root)?;

    let probe = RealHost;
    // A representative mask path; the executor creates the real one per session.
    let mask = PathBuf::from("<mask: empty read-only file>");
    let inputs = PlanInputs {
        root: &root,
        security: &security,
        grants: &[],
        mask_file: &mask,
        relay_port: crate::sandbox::RELAY_PORT,
    };
    let plan = SandboxPlan::build(&inputs, &probe)?;
    let denylist = Denylist::build(&probe, &root);
    println!("project {}\n", root.display());
    print!("{}", plan.render(&denylist));
    Ok(())
}
