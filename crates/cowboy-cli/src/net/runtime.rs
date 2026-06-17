//! Agent container lifecycle: build/ensure the image, start a long-lived
//! container with the project mounted, and exec commands into it.
//!
//! Security boundary enforced here: the project is mounted at `/workspace`, but
//! the host-owned `security.yaml` (and `models.yaml`) are **masked** with an
//! empty read-only file so the agent cannot read them even though they live
//! under the mounted project directory.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cowboy_core::config::{self, SecurityConfig};

use super::docker::{BindMount, ContainerSpec, ContainerState, DockerCli, ExecResult};
use super::gateway::GatewayNetwork;

const DEFAULT_IMAGE: &str = "cowboy/agent:local";
/// The image's `MISE_DATA_DIR` (toolchain store). Keep in sync with
/// `docker/agent.Dockerfile`; a host cache is bind-mounted here to persist
/// installs across container recreations.
const MISE_DATA_DIR: &str = "/usr/local/share/mise";
/// Repo root baked in at build time; the default source root for building the
/// bundled images when `COWBOY_SRC` is not set.
const COMPILE_REPO_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

/// Orchestrates the agent container for a single project.
pub struct AgentRuntime {
    docker: Box<dyn DockerCli>,
    root: PathBuf,
    security: SecurityConfig,
    container_name: String,
    /// The agent runs as this `uid:gid` (the host user) so it isn't root and
    /// files it creates in the mounted workspace are owned by the user.
    user: Option<String>,
    /// Present when network isolation is enabled (the default).
    gateway: Option<GatewayNetwork>,
    /// TTL cache of resolved `source_command` secrets, so shell commands get a
    /// fresh-ish token without re-running the host command every time.
    secret_cache: std::sync::Mutex<Option<SecretCache>>,
}

/// Cached `source_command` secrets with the instant they were resolved (for TTL).
type SecretCache = (std::time::Instant, Vec<(String, String)>);

impl AgentRuntime {
    pub fn new(
        docker: Box<dyn DockerCli>,
        root: PathBuf,
        security: SecurityConfig,
    ) -> Result<Self> {
        // Allow pinning the container name (used by tests and advanced setups).
        let container_name = std::env::var("COWBOY_CONTAINER_NAME")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| container_name_for(&root));
        // Fail CLOSED: if network isolation is requested but the gateway can't be
        // built, refuse to run rather than silently dropping to an unsandboxed
        // container (default bridge, full egress, caps intact). `None` means
        // isolation was not requested, never "we gave up".
        let gateway = if security.networks.isolated.enabled {
            Some(
                GatewayNetwork::for_project(project_hash(&root), &security, &root).context(
                    "network isolation is enabled but the gateway could not be built; \
                     refusing to run the agent unsandboxed",
                )?,
            )
        } else {
            None
        };
        Ok(Self {
            docker,
            root,
            security,
            container_name,
            user: Some(host_user()),
            gateway,
            secret_cache: std::sync::Mutex::new(None),
        })
    }

    fn user(&self) -> &str {
        self.user.as_deref().unwrap_or("")
    }

    /// The stable container name for this project (also used to let a subagent
    /// reuse this session's container).
    pub fn container_name(&self) -> &str {
        &self.container_name
    }

    /// The host control address + token (`bridge_gateway:port`, token) the host
    /// binds its TCP control server on, when network isolation is enabled.
    pub fn control_endpoint(&self) -> Option<(String, String)> {
        self.gateway
            .as_ref()
            .map(|g| (g.control_addr().to_string(), g.control_token().to_string()))
    }

    /// The project root (for approval persistence).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Build the container spec, applying mounts and the security mask.
    pub fn build_spec(&self) -> Result<ContainerSpec> {
        let c = &self.security.container;
        let mut mounts = Vec::new();

        // Project mount(s) from config.
        for m in &c.mounts {
            let source = resolve_source(&self.root, &m.source)
                .to_string_lossy()
                .into_owned();
            mounts.push(if m.mode == "ro" {
                BindMount::ro(source, m.target.clone())
            } else {
                BindMount::rw(source, m.target.clone())
            });
        }

        // Git worktree support: when the project root is a linked worktree, its
        // `.git` is a *file* pointing into the main repo's git dir, which lives
        // OUTSIDE /workspace — so in-container git can't resolve it. Mount the
        // shared git common dir at its own host path so the absolute gitdir
        // reference resolves and git (status/diff/log/commit) works. rw so the
        // worktree branch can write objects/refs into the shared store.
        if let Some(common) = git_common_dir(&self.root) {
            let p = common.to_string_lossy().into_owned();
            mounts.push(BindMount::rw(p.clone(), p));
        }

        // Mask host-owned config that would otherwise be visible via the
        // project mount. NEVER let the agent read security.yaml.
        let mask = ensure_mask_file()?;
        let mask_str = mask.to_string_lossy().into_owned();
        for (file, host_path) in [
            (
                config::SECURITY_FILE,
                self.root
                    .join(config::COWBOY_DIR)
                    .join(config::SECURITY_FILE),
            ),
            (
                config::MODELS_FILE,
                self.root.join(config::COWBOY_DIR).join(config::MODELS_FILE),
            ),
        ] {
            if host_path.exists() {
                mounts.push(BindMount::ro(
                    mask_str.clone(),
                    format!("{}/{}/{}", c.workdir, config::COWBOY_DIR, file),
                ));
            }
        }

        // Run non-root as the host user; HOME=/tmp is writable for any uid
        // (CARGO_HOME/RUSTUP_HOME are world-writable in the image).
        let mut env: Vec<(String, String)> = vec![("HOME".into(), "/tmp".into())];

        // Static secret env injected at container creation, sourced from a host
        // env var. `source_command` secrets are NOT injected here — they're
        // resolved fresh per shell command in `exec_stream` (so short-lived
        // tokens refresh mid-session). Never logged.
        for secret in &self.security.secrets.env {
            if secret.source_command.is_some() || secret.source_env.is_empty() {
                continue;
            }
            match std::env::var(&secret.source_env) {
                Ok(value) => env.push((secret.name.clone(), value)),
                Err(_) if secret.required => {
                    return Err(anyhow::anyhow!(
                        "required secret {} missing (set ${} on the host)",
                        secret.name,
                        secret.source_env
                    ));
                }
                Err(_) => {} // optional and unset: skip
            }
        }

        // Host credential grants (read-only by default), mounted where the
        // agent's CLIs look (HOME=/tmp). Missing optional grants are skipped.
        for grant in &self.security.secrets.files {
            let source = config::expand_path(&grant.source)
                .with_context(|| format!("credential grant {}", grant.source))?;
            if !source.exists() {
                if grant.required {
                    return Err(anyhow::anyhow!(
                        "required credential {} is missing on the host",
                        source.display()
                    ));
                }
                continue; // optional and absent: skip
            }
            let src = source.to_string_lossy().into_owned();
            mounts.push(if grant.read_only {
                BindMount::ro(src, grant.target.clone())
            } else {
                BindMount::rw(src, grant.target.clone())
            });
        }

        // Persist mise's toolchain store (downloads/installs/shims) across
        // container recreations: bind-mount a host cache dir over the image's
        // MISE_DATA_DIR so `mise install` doesn't re-download the project's
        // toolchain on every fresh container. Shared across projects — mise's
        // store is version-keyed, so toolchains dedupe and a repeated version is
        // reused. Host-owned (so the non-root agent can write it; a docker named
        // volume would be root-owned and unwritable). Best-effort: if the cache
        // dir can't be resolved/created, fall back to the ephemeral image dir.
        if let Some(cache) = config::global_cache_dir().map(|c| c.join("mise")) {
            if std::fs::create_dir_all(&cache).is_ok() {
                mounts.push(BindMount::rw(
                    cache.to_string_lossy().into_owned(),
                    MISE_DATA_DIR.to_string(),
                ));
            }
        }

        // When isolation is enabled, attach the agent to the internal-only
        // network, point DNS at the gateway, drop NET_ADMIN/NET_RAW so the
        // agent cannot change its route, and disable IPv6.
        let (network, ip, dns, cap_drop, sysctls) = match &self.gateway {
            Some(gw) => (
                Some(gw.internal_net.clone()),
                None,
                vec![gw.gateway_ip.clone()],
                gw.agent_caps(),
                vec![("net.ipv6.conf.all.disable_ipv6".into(), "1".into())],
            ),
            None => (None, None, Vec::new(), Vec::new(), Vec::new()),
        };

        Ok(ContainerSpec {
            name: self.container_name.clone(),
            image: c.image.clone(),
            workdir: c.workdir.clone(),
            mounts,
            env,
            network,
            ip,
            memory: c.memory.clone(),
            cpus: c.cpus,
            cap_drop,
            cap_add: Vec::new(),
            sysctls,
            dns,
            user: self.user.clone(),
            entrypoint: None,
            keep_alive: None,
        })
    }

    /// Ensure the configured image is available, building or pulling as needed.
    pub async fn ensure_image(&self) -> Result<()> {
        let image = &self.security.container.image;
        if self.docker.image_exists(image).await? {
            return Ok(());
        }

        // Explicit dockerfile in config wins.
        if let Some(df) = &self.security.container.dockerfile {
            let dockerfile = resolve_source(&self.root, df);
            tracing::info!(%image, dockerfile = %dockerfile.display(), "building agent image");
            return self
                .docker
                .build_image(&dockerfile, &self.root, image)
                .await;
        }

        // The bundled default image is built from the cowboy source tree.
        if image == DEFAULT_IMAGE {
            if let Some(src) = default_image_source_root() {
                let dockerfile = src.join("docker").join("agent.Dockerfile");
                tracing::info!(%image, src = %src.display(),
                    "building bundled agent image (first run; this may take a few minutes)");
                return self.docker.build_image(&dockerfile, &src, image).await;
            }
            anyhow::bail!(
                "agent image {DEFAULT_IMAGE} not found and no source tree to build it from.\n\
                 Build it with `docker/build.sh agent` (or set COWBOY_SRC to the cowboy repo)."
            );
        }

        // Otherwise assume it's a registry image.
        tracing::info!(%image, "pulling agent image");
        self.docker.pull_image(image).await
    }

    /// Ensure a long-lived agent container is running, creating or starting it.
    pub async fn ensure_running(&self) -> Result<()> {
        match self.docker.container_state(&self.container_name).await? {
            ContainerState::Running => Ok(()),
            ContainerState::Stopped => self.docker.start(&self.container_name).await,
            ContainerState::Absent => self.create().await,
        }
    }

    async fn create(&self) -> Result<()> {
        self.ensure_image().await?;
        // Bring the gateway up before the agent so the route helper can run.
        if let Some(gw) = &self.gateway {
            gw.ensure(&*self.docker).await?;
        }
        let spec = self.build_spec()?;
        self.docker.run_detached(&spec).await?;

        if let Some(gw) = &self.gateway {
            // Force the agent's default route through the gateway (the agent
            // lacks NET_ADMIN, so it cannot undo this).
            gw.force_agent_route(&*self.docker, &self.container_name)
                .await?;
            // Attach any approved Compose networks (traffic to these bypasses
            // the gateway via the agent's own NIC).
            for net in &self.security.networks.compose.approved {
                self.docker
                    .connect_network(net, &self.container_name)
                    .await?;
            }
        }
        Ok(())
    }

    /// mise config files we recognize at the workspace root.
    const MISE_CONFIGS: &'static [&'static str] = &[
        "mise.toml",
        ".mise.toml",
        "mise/config.toml",
        ".mise/config.toml",
        ".config/mise/config.toml",
        ".tool-versions",
    ];

    /// Whether the workspace declares dev dependencies via mise. The agent loop
    /// uses this to run a *visible* `mise install` at session start (so the
    /// toolchain setup streams to the UI instead of silently delaying the first
    /// request).
    pub fn has_mise_config(&self) -> bool {
        Self::MISE_CONFIGS
            .iter()
            .any(|f| self.root.join(f).exists())
    }

    /// Run a command inside the container, streaming output, returning its exit code.
    pub async fn run(&self, argv: &[String]) -> Result<ExecResult> {
        self.ensure_running().await?;
        self.docker
            .exec(
                &self.container_name,
                &self.security.container.workdir,
                self.user(),
                argv,
            )
            .await
    }

    /// Run a shell command, streaming combined output to `chunks` as it arrives,
    /// interruptible via `cancel` and bounded by `timeout_secs` (group-killed in
    /// the container on either). Returns (exit, full output). For the agent loop.
    pub async fn exec_stream(
        &self,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: u64,
        cancel: tokio_util::sync::CancellationToken,
        chunks: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Result<(ExecResult, String)> {
        self.ensure_running().await?;
        let workdir = cwd.unwrap_or(&self.security.container.workdir);
        // Inject `source_command` secrets fresh (TTL-cached) into shell commands
        // — e.g. `GH_TOKEN` from `gh auth token` — so short-lived tokens refresh
        // mid-session without recreating the container. Exported in the command's
        // own shell (the agent is the intended recipient).
        let prefixed;
        let command = match self.dynamic_secret_exports() {
            exports if exports.is_empty() => command,
            exports => {
                prefixed = format!("{exports}{command}");
                &prefixed
            }
        };
        self.docker
            .exec_stream(
                &self.container_name,
                workdir,
                self.user(),
                command,
                timeout_secs,
                cancel,
                chunks,
            )
            .await
    }

    /// `export NAME='value'; ` lines for every `source_command` secret, resolved
    /// on the host and cached briefly. Empty when there are none.
    fn dynamic_secret_exports(&self) -> String {
        let mut out = String::new();
        for (name, value) in self.dynamic_secret_env() {
            out.push_str(&format!("export {name}={}; ", sh_quote(&value)));
        }
        out
    }

    /// Resolve `source_command` secrets (run their host commands), cached for a
    /// short TTL so we don't re-run them on every shell command.
    fn dynamic_secret_env(&self) -> Vec<(String, String)> {
        const TTL: std::time::Duration = std::time::Duration::from_secs(60);
        let mut cache = self.secret_cache.lock().unwrap();
        if let Some((at, vals)) = cache.as_ref() {
            if at.elapsed() < TTL {
                return vals.clone();
            }
        }
        let vals: Vec<(String, String)> = self
            .security
            .secrets
            .env
            .iter()
            .filter_map(|s| {
                let cmd = s.source_command.as_deref()?.trim();
                (!cmd.is_empty())
                    .then(|| run_value_command(cmd).map(|v| (s.name.clone(), v)))
                    .flatten()
            })
            .collect();
        *cache = Some((std::time::Instant::now(), vals.clone()));
        vals
    }

    /// Run a structured file operation inside the container via the in-container
    /// `cowboy x-fileop` helper, passing the JSON `payload` on stdin (so
    /// multi-line content needs no shell quoting). Returns (exit, output). The
    /// op runs confined by Docker — file edits never touch the host directly.
    pub async fn fileop(&self, payload: &str) -> Result<(ExecResult, String)> {
        self.ensure_running().await?;
        let argv = vec!["cowboy".to_string(), "x-fileop".to_string()];
        self.docker
            .exec_stdin(
                &self.container_name,
                &self.security.container.workdir,
                self.user(),
                &argv,
                payload,
            )
            .await
    }

    /// Stop all managed processes (kill their process groups) in the container,
    /// best-effort. Called on session exit so no services linger.
    pub async fn stop_all_processes(&self) -> Result<()> {
        if self.docker.container_state(&self.container_name).await?
            != crate::net::docker::ContainerState::Running
        {
            return Ok(());
        }
        let dir = format!("{}/.cowboy/proc", self.security.container.workdir);
        let script = format!(
            "for f in {dir}/*.pid; do [ -f \"$f\" ] && \
             kill -TERM -\"$(cat \"$f\")\" 2>/dev/null; done; true"
        );
        let argv = vec!["sh".to_string(), "-c".to_string(), script];
        let _ = self
            .docker
            .exec_capture(
                &self.container_name,
                &self.security.container.workdir,
                self.user(),
                &argv,
            )
            .await;
        Ok(())
    }

    /// Run a shell command string inside the container, capturing combined
    /// output (for the agent loop). The command runs via `sh -lc` and is bounded
    /// by `timeout_secs` (0 = no timeout); on timeout the local exec client is
    /// killed and a timeout observation is returned.
    pub async fn run_capture(
        &self,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: u64,
    ) -> Result<(ExecResult, String)> {
        self.ensure_running().await?;
        let workdir = cwd.unwrap_or(&self.security.container.workdir);
        // Non-login `sh -c` so the container's ENV PATH (rust/go toolchains) is
        // inherited; a login shell would reset PATH via /etc/profile.
        let argv = vec!["sh".to_string(), "-c".to_string(), command.to_string()];
        let fut = self
            .docker
            .exec_capture(&self.container_name, workdir, self.user(), &argv);
        if timeout_secs == 0 {
            return fut.await;
        }
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), fut).await {
            Ok(res) => res,
            Err(_) => Ok((
                ExecResult { exit_code: 124 },
                format!("[command timed out after {timeout_secs}s]"),
            )),
        }
    }

    /// Open an interactive shell inside the container.
    pub async fn shell(&self) -> Result<ExecResult> {
        self.ensure_running().await?;
        let argv = vec!["bash".to_string()];
        self.docker
            .exec_interactive(
                &self.container_name,
                &self.security.container.workdir,
                self.user(),
                &argv,
            )
            .await
    }
}

/// The host user as `uid:gid`, so the container runs non-root and writes files
/// owned by the user.
fn host_user() -> String {
    // SAFETY: getuid/getgid are always-safe libc calls.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    format!("{uid}:{gid}")
}

/// A stable 32-bit hash of the project path, used to derive per-project network
/// names and subnets.
pub fn project_hash(root: &Path) -> u32 {
    let mut hasher = DefaultHasher::new();
    root.hash(&mut hasher);
    hasher.finish() as u32
}

/// POSIX single-quote a value for safe `export VAR=<value>` in a shell.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Run a host command and return its trimmed stdout as a secret value, or
/// `None` if it fails / produces nothing / exceeds the timeout. Used for
/// keyring-backed tokens (`gh auth token`). The command comes from host-owned
/// config; never logged.
///
/// stdin is `/dev/null` so a credential helper that would otherwise prompt
/// interactively fails fast instead of blocking, and a bounded timeout backstops
/// anything that still hangs — this runs (cached) on every shell exec, so a hang
/// here would otherwise deadlock the whole session.
fn run_value_command(cmd: &str) -> Option<String> {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    let cmd = cmd.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .stdin(std::process::Stdio::null())
            .output();
        let _ = tx.send(out);
    });
    let out = rx.recv_timeout(TIMEOUT).ok()?.ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!v.is_empty()).then_some(v)
}

/// Whether a `source_command` produces a value on the host, using the same
/// bounded/`stdin`-null execution as the live path (so `cowboy secrets list`
/// can't hang on an interactive credential helper). Does not expose the value.
pub(crate) fn source_command_ok(cmd: &str) -> bool {
    run_value_command(cmd).is_some()
}

/// The repository root that's shared by every worktree: `git rev-parse
/// --git-common-dir` resolves to `<main-repo>/.git` from both a normal checkout
/// and a linked worktree, so its parent is the one repo they share. Falls back
/// to `root` for a non-git directory.
pub fn repo_root(root: &Path) -> PathBuf {
    if let Ok(out) = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
    {
        if out.status.success() {
            let common = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
            if let Some(parent) = common.parent() {
                if !parent.as_os_str().is_empty() {
                    return parent.to_path_buf();
                }
            }
        }
    }
    root.to_path_buf()
}

/// The per-repository overlay key (stable across all of a repo's worktrees).
/// Used for the personal credential overlay so a grant applies to every worktree.
pub fn repo_key(root: &Path) -> String {
    format!("{:08x}", project_hash(&repo_root(root)))
}

/// Derive a stable, unique container name from the project path.
pub fn container_name_for(root: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    root.hash(&mut hasher);
    let hash = hasher.finish();
    let slug: String = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(24)
        .collect();
    format!("cowboy-agent-{slug}-{:08x}", hash as u32)
}

/// Resolve a mount source relative to the project root (`.` -> root).
fn resolve_source(root: &Path, source: &str) -> PathBuf {
    let p = Path::new(source);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        // Canonicalize so docker gets an absolute host path.
        let joined = root.join(p);
        std::fs::canonicalize(&joined).unwrap_or(joined)
    }
}

/// The shared git directory to mount when `root` is a *linked worktree* — i.e.
/// `<root>/.git` is a file (a `gitdir:` pointer into the main repo) rather than
/// a directory. Returns the main repo's git common dir (e.g. `<main>/.git`),
/// which lives outside `<root>` and must be mounted at its own absolute path so
/// the worktree's gitdir reference resolves in the container. `None` for a
/// normal repo (its `.git` dir is already inside the workspace mount) or a
/// non-git directory.
fn git_common_dir(root: &Path) -> Option<PathBuf> {
    // Only linked worktrees have a `.git` *file*; a normal repo has a directory
    // that's already covered by the /workspace mount.
    if !root.join(".git").is_file() {
        return None;
    }
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    // It lives outside the workspace by definition; guard anyway, and require
    // that it actually exists on the host.
    if dir.as_os_str().is_empty() || dir.starts_with(root) || !dir.exists() {
        return None;
    }
    Some(dir)
}

/// Create (once) an empty file used to mask host-owned config inside the container.
fn ensure_mask_file() -> Result<PathBuf> {
    let path = std::env::temp_dir().join("cowboy-mask-empty");
    if !path.exists() {
        std::fs::write(&path, b"")
            .with_context(|| format!("creating mask file {}", path.display()))?;
    }
    Ok(path)
}

/// Locate the cowboy source tree for building the bundled images: `COWBOY_SRC`
/// if set, else the repo root baked in at compile time. Returns None if neither
/// contains `docker/agent.Dockerfile`.
fn default_image_source_root() -> Option<PathBuf> {
    let candidates = [
        std::env::var("COWBOY_SRC").ok().map(PathBuf::from),
        Some(PathBuf::from(COMPILE_REPO_ROOT)),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|p| p.join("docker").join("agent.Dockerfile").exists())
        .and_then(|p| std::fs::canonicalize(p).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::docker::MockDockerCli;
    use cowboy_core::config::Mount;

    /// Build a runtime over a temp project dir with `security.yaml` + `models.yaml`
    /// present, so the mask logic has something to mask. `isolated` toggles the
    /// network gateway.
    fn fixture(isolated: bool, docker: MockDockerCli) -> (AgentRuntime, assert_fs::TempDir) {
        let tmp = assert_fs::TempDir::new().unwrap();
        let cowboy = tmp.path().join(".cowboy");
        std::fs::create_dir_all(&cowboy).unwrap();
        std::fs::write(cowboy.join("security.yaml"), "version: 1\n").unwrap();
        std::fs::write(cowboy.join("models.yaml"), "version: 1\n").unwrap();
        std::fs::write(cowboy.join("agent.yaml"), "version: 1\n").unwrap();

        let mut security = SecurityConfig {
            container: cowboy_core::config::ContainerConfig {
                image: "test/img:local".into(),
                mounts: vec![Mount {
                    source: ".".into(),
                    target: "/workspace".into(),
                    mode: "rw".into(),
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        security.networks.isolated.enabled = isolated;
        let rt = AgentRuntime::new(Box::new(docker), tmp.path().to_path_buf(), security)
            .expect("runtime fixture");
        (rt, tmp)
    }

    #[test]
    fn build_spec_masks_host_owned_config() {
        let (rt, _tmp) = fixture(false, MockDockerCli::new());
        let spec = rt.build_spec().unwrap();

        // The project is mounted rw at /workspace.
        assert!(spec
            .mounts
            .iter()
            .any(|m| m.target == "/workspace" && !m.read_only));

        // security.yaml and models.yaml are masked read-only by the empty file.
        let sec = spec
            .mounts
            .iter()
            .find(|m| m.target == "/workspace/.cowboy/security.yaml")
            .expect("security.yaml must be masked");
        assert!(sec.read_only, "mask must be read-only");
        assert!(sec.source.contains("cowboy-mask-empty"));
        assert!(spec
            .mounts
            .iter()
            .any(|m| m.target == "/workspace/.cowboy/models.yaml" && m.read_only));

        // agent.yaml is NOT masked — the agent may read/edit it.
        assert!(!spec.mounts.iter().any(|m| m.target.ends_with("agent.yaml")));

        // The mise toolchain store is persisted via a writable host cache mount
        // (when a home cache dir is resolvable — true in the test environment).
        if cowboy_core::config::global_cache_dir().is_some() {
            assert!(
                spec.mounts
                    .iter()
                    .any(|m| m.target == MISE_DATA_DIR && !m.read_only),
                "mise data dir should be a writable cache mount"
            );
        }
    }

    #[test]
    fn build_spec_isolated_drops_caps_and_points_dns_at_gateway() {
        let (rt, _tmp) = fixture(true, MockDockerCli::new());
        let spec = rt.build_spec().unwrap();

        // Agent is on the internal network with NET_ADMIN/NET_RAW dropped.
        assert!(spec.network.as_deref().unwrap().starts_with("cowboy-int-"));
        assert!(spec.cap_drop.contains(&"NET_ADMIN".to_string()));
        assert!(spec.cap_drop.contains(&"NET_RAW".to_string()));
        // DNS points at the gateway IP (10.88.x.2).
        assert_eq!(spec.dns.len(), 1);
        assert!(spec.dns[0].starts_with("10.88.") && spec.dns[0].ends_with(".2"));
        // IPv6 disabled.
        assert!(spec
            .sysctls
            .iter()
            .any(|(k, v)| k == "net.ipv6.conf.all.disable_ipv6" && v == "1"));
    }

    #[test]
    fn build_spec_injects_present_secrets_and_skips_missing() {
        use cowboy_core::config::SecretEnv;
        // SAFETY: unique var name; single-threaded within this test's logic.
        std::env::set_var("COWBOY_TEST_SECRET_SRC", "s3cr3t");
        let (mut rt, _tmp) = fixture(false, MockDockerCli::new());
        // Inject secrets into the runtime's security config.
        rt.security.secrets.env = vec![
            SecretEnv {
                name: "DB_URL".into(),
                source_env: "COWBOY_TEST_SECRET_SRC".into(),
                source_command: None,
                required: false,
                approval: None,
            },
            SecretEnv {
                name: "MISSING".into(),
                source_env: "COWBOY_TEST_SECRET_ABSENT".into(),
                source_command: None,
                required: false,
                approval: None,
            },
        ];
        let spec = rt.build_spec().unwrap();
        assert!(spec.env.iter().any(|(k, v)| k == "DB_URL" && v == "s3cr3t"));
        assert!(!spec.env.iter().any(|(k, _)| k == "MISSING"));
        std::env::remove_var("COWBOY_TEST_SECRET_SRC");
    }

    #[test]
    fn source_command_secret_resolves_per_exec_not_at_creation() {
        use cowboy_core::config::SecretEnv;
        let (mut rt, _tmp) = fixture(false, MockDockerCli::new());
        rt.security.secrets.env = vec![SecretEnv {
            name: "TOK".into(),
            source_env: String::new(),
            source_command: Some("printf 'tok-123'".into()),
            required: false,
            approval: None,
        }];
        // Not injected at container creation (refreshed per shell command instead).
        let spec = rt.build_spec().unwrap();
        assert!(!spec.env.iter().any(|(k, _)| k == "TOK"));
        // Resolved on demand, as a shell-safe export prefix.
        assert_eq!(
            rt.dynamic_secret_env(),
            vec![("TOK".into(), "tok-123".into())]
        );
        assert_eq!(rt.dynamic_secret_exports(), "export TOK='tok-123'; ");
    }

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("plain"), "'plain'");
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn run_value_command_trims_success_and_drops_failures() {
        assert_eq!(
            run_value_command("printf '  tok-123  '").as_deref(),
            Some("tok-123")
        );
        assert_eq!(run_value_command("exit 1"), None); // nonzero exit
        assert_eq!(run_value_command("true"), None); // success, no output
                                                     // stdin is /dev/null, so a helper that reads stdin gets EOF and returns
                                                     // empty instead of hanging the session (fast — no timeout wait).
        assert_eq!(run_value_command("cat"), None);
    }

    #[test]
    fn build_spec_mounts_credential_grants_read_only_and_skips_absent() {
        use cowboy_core::config::SecretMount;
        let (mut rt, tmp) = fixture(false, MockDockerCli::new());
        let cred = tmp.path().join("gh-config");
        std::fs::write(&cred, "token").unwrap();
        rt.security.secrets.files = vec![
            SecretMount {
                source: cred.to_string_lossy().into_owned(),
                target: "/tmp/.config/gh".into(),
                read_only: true,
                required: false,
                approval: None,
            },
            SecretMount {
                source: "/no/such/optional/cred".into(),
                target: "/tmp/.config/absent".into(),
                read_only: true,
                required: false,
                approval: None,
            },
        ];
        let spec = rt.build_spec().unwrap();
        let m = spec
            .mounts
            .iter()
            .find(|m| m.target == "/tmp/.config/gh")
            .expect("granted credential should be mounted");
        assert!(m.read_only, "credential mounts default read-only");
        assert!(m.source.contains("gh-config"));
        // An absent optional grant is silently skipped.
        assert!(!spec
            .mounts
            .iter()
            .any(|m| m.target == "/tmp/.config/absent"));
    }

    #[test]
    fn build_spec_errors_on_missing_required_credential() {
        use cowboy_core::config::SecretMount;
        let (mut rt, _tmp) = fixture(false, MockDockerCli::new());
        rt.security.secrets.files = vec![SecretMount {
            source: "/no/such/required/cred".into(),
            target: "/tmp/.config/x".into(),
            read_only: true,
            required: true,
            approval: None,
        }];
        assert!(rt.build_spec().is_err());
    }

    #[tokio::test]
    async fn ensure_running_starts_a_stopped_container() {
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Stopped));
        docker.expect_start().times(1).returning(|_| Ok(()));
        let (rt, _tmp) = fixture(false, docker);
        rt.ensure_running().await.unwrap();
    }

    #[tokio::test]
    async fn ensure_running_creates_when_absent_building_image_if_missing() {
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Absent));
        docker.expect_image_exists().returning(|_| Ok(false));
        docker.expect_pull_image().times(1).returning(|_| Ok(()));
        docker.expect_run_detached().times(1).returning(|_| Ok(()));
        let (rt, _tmp) = fixture(false, docker);
        rt.ensure_running().await.unwrap();
    }

    #[tokio::test]
    async fn isolated_create_brings_up_gateway_and_forces_route() {
        let mut docker = MockDockerCli::new();
        // Agent absent -> create path. Gateway image + agent image present.
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Absent));
        docker.expect_image_exists().returning(|_| Ok(true));
        docker.expect_network_exists().returning(|_| Ok(false));
        // Both networks (internal + egress) get created.
        docker
            .expect_create_network()
            .times(2)
            .returning(|_| Ok(()));
        // Gateway + agent containers launched.
        docker.expect_run_detached().times(2).returning(|_| Ok(()));
        // Egress network attached to the gateway.
        docker
            .expect_connect_network()
            .times(1)
            .returning(|_, _| Ok(()));
        // The route-forcing helper MUST run (the core of the boundary).
        docker
            .expect_run_oneshot()
            .times(1)
            .returning(|_| Ok(ExecResult { exit_code: 0 }));
        let (rt, _tmp) = fixture(true, docker);
        rt.ensure_running().await.unwrap();
    }

    #[tokio::test]
    async fn run_execs_in_workspace_with_stable_name() {
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_exec()
            .withf(|_name, workdir, _user, argv| workdir == "/workspace" && argv == ["pwd"])
            .times(1)
            .returning(|_, _, _, _| Ok(ExecResult { exit_code: 0 }));
        let (rt, _tmp) = fixture(false, docker);
        assert!(rt.container_name().starts_with("cowboy-agent-"));
        let res = rt.run(&["pwd".to_string()]).await.unwrap();
        assert_eq!(res.exit_code, 0);
    }

    #[test]
    fn container_name_is_stable_and_sanitized() {
        let a = container_name_for(Path::new("/home/dev/projects/My App"));
        let b = container_name_for(Path::new("/home/dev/projects/My App"));
        assert_eq!(a, b, "name must be stable for a path");
        assert!(a.starts_with("cowboy-agent-myapp-"));
        // No spaces or uppercase leak into the docker name.
        assert!(!a.contains(' '));
        assert_eq!(a, a.to_lowercase());
    }

    #[test]
    fn distinct_paths_get_distinct_names() {
        let a = container_name_for(Path::new("/a/project"));
        let b = container_name_for(Path::new("/b/project"));
        assert_ne!(a, b);
    }

    fn git(args: &[&str], cwd: &Path) -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn git_common_dir_only_set_for_worktrees() {
        // Self-skip when git is unavailable.
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let base = std::env::temp_dir().join(format!("cowboy-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let main = base.join("main");
        std::fs::create_dir_all(&main).unwrap();
        assert!(git(&["init", "-q"], &main));
        assert!(git(&["config", "user.email", "t@t"], &main));
        assert!(git(&["config", "user.name", "t"], &main));
        std::fs::write(main.join("f.txt"), "hi").unwrap();
        assert!(git(&["add", "."], &main));
        assert!(git(&["commit", "-qm", "init"], &main));

        // A normal repo: `.git` is a directory → no extra mount needed.
        assert!(git_common_dir(&main).is_none());

        // A linked worktree: `.git` is a file → mount the main repo's git dir.
        let wt = base.join("wt");
        assert!(git(
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "feature"
            ],
            &main,
        ));
        assert!(wt.join(".git").is_file());
        let common = git_common_dir(&wt).expect("worktree → common git dir");
        // It points at the main repo's .git, outside the worktree.
        assert!(common.ends_with(".git"));
        assert!(!common.starts_with(&wt));
        assert!(common.is_dir());

        let _ = std::fs::remove_dir_all(&base);
    }
}
