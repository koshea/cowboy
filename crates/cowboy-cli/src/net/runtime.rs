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

/// Embedded default image sources, so `cowboy` can build `cowboy/agent:local`
/// with no registry or external files.
const EMBEDDED_DOCKERFILE: &str = include_str!("../../../../docker/agent.Dockerfile");
const EMBEDDED_ENTRYPOINT: &str = include_str!("../../../../docker/agent-entrypoint.sh");
const DEFAULT_IMAGE: &str = "cowboy/agent:local";

/// Orchestrates the agent container for a single project.
pub struct AgentRuntime {
    docker: Box<dyn DockerCli>,
    root: PathBuf,
    security: SecurityConfig,
    container_name: String,
    /// Present when network isolation is enabled (the default).
    gateway: Option<GatewayNetwork>,
}

impl AgentRuntime {
    pub fn new(docker: Box<dyn DockerCli>, root: PathBuf, security: SecurityConfig) -> Self {
        // Allow pinning the container name (used by tests and advanced setups).
        let container_name = std::env::var("COWBOY_CONTAINER_NAME")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| container_name_for(&root));
        let gateway = if security.networks.isolated.enabled {
            GatewayNetwork::for_project(project_hash(&root), &security).ok()
        } else {
            None
        };
        Self {
            docker,
            root,
            security,
            container_name,
            gateway,
        }
    }

    /// The stable container name for this project (used in diagnostics/tests).
    #[allow(dead_code)]
    pub fn container_name(&self) -> &str {
        &self.container_name
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

        // Explicit, host-configured secret env injection. Values are read from
        // the host env var named by `source_env` and never logged.
        let mut env: Vec<(String, String)> = Vec::new();
        for secret in &self.security.secrets.env {
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

        // The bundled default image is built from embedded sources.
        if image == DEFAULT_IMAGE {
            tracing::info!(%image, "building bundled agent image (first run; this may take a while)");
            let ctx = write_embedded_context()?;
            return self
                .docker
                .build_image(&ctx.join("agent.Dockerfile"), &ctx, image)
                .await;
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

    /// Run a command inside the container, streaming output, returning its exit code.
    pub async fn run(&self, argv: &[String]) -> Result<ExecResult> {
        self.ensure_running().await?;
        self.docker
            .exec(&self.container_name, &self.security.container.workdir, argv)
            .await
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
        let argv = vec!["sh".to_string(), "-lc".to_string(), command.to_string()];
        let fut = self
            .docker
            .exec_capture(&self.container_name, workdir, &argv);
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
                &argv,
            )
            .await
    }
}

/// A stable 32-bit hash of the project path, used to derive per-project network
/// names and subnets.
fn project_hash(root: &Path) -> u32 {
    let mut hasher = DefaultHasher::new();
    root.hash(&mut hasher);
    hasher.finish() as u32
}

/// Derive a stable, unique container name from the project path.
fn container_name_for(root: &Path) -> String {
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

/// Create (once) an empty file used to mask host-owned config inside the container.
fn ensure_mask_file() -> Result<PathBuf> {
    let path = std::env::temp_dir().join("cowboy-mask-empty");
    if !path.exists() {
        std::fs::write(&path, b"")
            .with_context(|| format!("creating mask file {}", path.display()))?;
    }
    Ok(path)
}

/// Write the embedded Dockerfile + entrypoint (+ a stub helper) to a temp build
/// context and return its path.
fn write_embedded_context() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("cowboy-agent-build");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("agent.Dockerfile"), EMBEDDED_DOCKERFILE)?;
    std::fs::write(dir.join("agent-entrypoint.sh"), EMBEDDED_ENTRYPOINT)?;
    // Placeholder for the in-container helper until Slice F ships the real one.
    let stub = "#!/bin/sh\necho 'cowboy helper: not yet bundled in this image' >&2\nexit 1\n";
    std::fs::write(dir.join("cowboy-agent"), stub)?;
    Ok(dir)
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
        let rt = AgentRuntime::new(Box::new(docker), tmp.path().to_path_buf(), security);
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
                required: false,
                approval: None,
            },
            SecretEnv {
                name: "MISSING".into(),
                source_env: "COWBOY_TEST_SECRET_ABSENT".into(),
                required: false,
                approval: None,
            },
        ];
        let spec = rt.build_spec().unwrap();
        assert!(spec.env.iter().any(|(k, v)| k == "DB_URL" && v == "s3cr3t"));
        assert!(!spec.env.iter().any(|(k, _)| k == "MISSING"));
        std::env::remove_var("COWBOY_TEST_SECRET_SRC");
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
            .withf(|_name, workdir, argv| workdir == "/workspace" && argv == ["pwd"])
            .times(1)
            .returning(|_, _, _| Ok(ExecResult { exit_code: 0 }));
        let (rt, _tmp) = fixture(false, docker);
        assert!(rt.container_name().starts_with("cowboy-agent-"));
        let res = rt.run(&["pwd".to_string()]).await.unwrap();
        assert_eq!(res.exit_code, 0);
    }

    #[test]
    fn container_name_is_stable_and_sanitized() {
        let a = container_name_for(Path::new("/home/kevin/dev/My App"));
        let b = container_name_for(Path::new("/home/kevin/dev/My App"));
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
}
