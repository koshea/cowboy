//! Docker control.
//!
//! The lifecycle/network/exec operations talk to the Docker daemon through the
//! typed [`bollard`] API (no arg-quoting or output-parsing fragility, one HTTP
//! connection instead of a process per call). Two operations stay as `docker`
//! CLI shell-outs on purpose — `build_image` (the CLI tars the context and
//! honors `.dockerignore` for free) and `exec_interactive` (the CLI handles
//! terminal raw-mode + window-resize for free) — neither passes user data
//! through a shell, so there is no fragility to remove there. Everything sits
//! behind the [`DockerCli`] trait, which is mockable (`mockall`) for unit tests.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use bollard::container::LogOutput;
use bollard::errors::Error as BollardError;
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::models::{
    ContainerCreateBody, EndpointIpamConfig, EndpointSettings, HostConfig, Ipam, IpamConfig,
    NetworkConnectRequest, NetworkCreateRequest, NetworkingConfig,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptionsBuilder,
    ListNetworksOptionsBuilder, RemoveContainerOptionsBuilder,
};
use bollard::Docker;
use futures::StreamExt;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::process::Command;
use tokio::sync::OnceCell;

/// Monotonic counter for unique per-exec tags. Each streaming exec gets its own
/// pidfile name and env marker so concurrent execs (e.g. parallel subagents, or a
/// foreman running alongside them) never clobber each other's kill target — a
/// shared pidfile would make cancel/timeout signal the wrong process group.
static EXEC_SEQ: AtomicU64 = AtomicU64::new(0);

/// A process-unique tag for one streamed exec, used as both its pidfile name and
/// the `COWBOY_EXEC_TAG` env marker. `pid-seq` is unique within this process; a
/// subagent is a separate process (distinct pid), so it can't collide either.
fn next_exec_tag() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        EXEC_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Shell run (as a one-shot exec) to terminate a streamed command on
/// cancel/timeout. Two layers, TERM then — after a short grace — KILL:
/// (a) signal the recorded process group (fast, the common case); (b) sweep
/// `/proc` for any process still carrying this exec's `COWBOY_EXEC_TAG` and signal
/// it too — catching descendants that re-`setsid` into a fresh group and so
/// escaped (a) (e.g. a `mise` hook). Finally removes the per-exec pidfile.
fn kill_exec_script(pidfile: &str, tag: &str) -> String {
    let sweep = |sig: &str| {
        format!(
            "for e in /proc/[0-9]*/environ; do \
               grep -qz 'COWBOY_EXEC_TAG={tag}' \"$e\" 2>/dev/null && \
               kill -{sig} \"$(basename \"$(dirname \"$e\")\")\" 2>/dev/null; \
             done"
        )
    };
    format!(
        "p=$(cat {pidfile} 2>/dev/null); \
         [ -n \"$p\" ] && kill -TERM -\"$p\" 2>/dev/null; {term_sweep}; \
         sleep 1; \
         [ -n \"$p\" ] && kill -KILL -\"$p\" 2>/dev/null; {kill_sweep}; \
         rm -f {pidfile} 2>/dev/null; true",
        term_sweep = sweep("TERM"),
        kill_sweep = sweep("KILL"),
    )
}

/// A bind mount applied to the agent container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindMount {
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

impl BindMount {
    pub fn rw(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            read_only: false,
        }
    }
    pub fn ro(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            read_only: true,
        }
    }
    /// Render as a `docker run -v` value.
    pub fn to_arg(&self) -> String {
        if self.read_only {
            format!("{}:{}:ro", self.source, self.target)
        } else {
            format!("{}:{}", self.source, self.target)
        }
    }
}

/// Specification for a container (agent, gateway, or one-shot helper).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub workdir: String,
    pub mounts: Vec<BindMount>,
    /// Non-secret environment variables (name, value).
    pub env: Vec<(String, String)>,
    /// Optional Docker network to attach at creation.
    pub network: Option<String>,
    /// Optional static IP within `network`.
    pub ip: Option<String>,
    pub memory: Option<String>,
    pub cpus: Option<f64>,
    /// Capabilities to drop (e.g. NET_ADMIN, NET_RAW). Applied as `--cap-drop`.
    pub cap_drop: Vec<String>,
    /// Capabilities to add (e.g. NET_ADMIN for the gateway). `--cap-add`.
    pub cap_add: Vec<String>,
    /// Kernel sysctls to set (`--sysctl`).
    pub sysctls: Vec<(String, String)>,
    /// DNS servers for the container (`--dns`).
    pub dns: Vec<String>,
    /// Extra `host:ip` entries (`--add-host`). Used to map `host.docker.internal`
    /// to the host gateway so the gateway can dial the host's control server on
    /// Docker Desktop (Mac/Windows), where the host has no docker-bridge IP.
    pub extra_hosts: Vec<String>,
    /// Run as this `uid:gid` (`--user`) — used to run the agent non-root.
    pub user: Option<String>,
    /// `--security-opt` values (e.g. `no-new-privileges`).
    pub security_opt: Vec<String>,
    /// `--pids-limit`: cap the number of processes (fork-bomb resilience).
    pub pids_limit: Option<u32>,
    /// Override the image `ENTRYPOINT` (`--entrypoint`).
    pub entrypoint: Option<String>,
    /// Command to run; defaults to `tail -f /dev/null` for keep-alive.
    pub keep_alive: Option<Vec<String>>,
}

/// Parameters for `docker network create`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkSpec {
    pub name: String,
    /// `--internal`: no route to the outside world.
    pub internal: bool,
    pub subnet: Option<String>,
    pub gateway: Option<String>,
}

/// Result of a streamed command execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecResult {
    pub exit_code: i32,
}

/// Docker operations cowboy needs. bollard-backed, behind a mockable trait.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait DockerCli: Send + Sync {
    async fn image_exists(&self, image: &str) -> Result<bool>;
    async fn build_image(&self, dockerfile: &Path, context: &Path, tag: &str) -> Result<()>;
    async fn pull_image(&self, image: &str) -> Result<()>;
    async fn container_state(&self, name: &str) -> Result<ContainerState>;
    /// The value of a container label (`None` if the container or label is
    /// absent). Used to detect a container left by a different cowboy version.
    async fn container_label(&self, name: &str, key: &str) -> Result<Option<String>>;
    async fn run_detached(&self, spec: &ContainerSpec) -> Result<()>;
    /// Run a one-shot container to completion (`docker run --rm`), returning its
    /// exit code. Used for the route-setup helper.
    async fn run_oneshot(&self, spec: &ContainerSpec) -> Result<ExecResult>;
    async fn start(&self, name: &str) -> Result<()>;
    /// Stop a running container (it can be `start`ed again). Used by idle teardown.
    async fn stop(&self, name: &str) -> Result<()>;
    /// Remove a container. Used by teardown paths (Slice C/G).
    #[allow(dead_code)]
    async fn remove(&self, name: &str, force: bool) -> Result<()>;
    /// True if a Docker network with this name exists.
    async fn network_exists(&self, name: &str) -> Result<bool>;
    /// Create a Docker network (idempotent callers check existence first).
    async fn create_network(&self, spec: &NetworkSpec) -> Result<()>;
    /// Connect an existing container to an additional network.
    async fn connect_network(&self, network: &str, container: &str) -> Result<()>;
    /// Remove a Docker network (ignores "not found").
    async fn remove_network(&self, name: &str) -> Result<()>;
    /// IDs of cowboy-labelled containers and networks (`(containers, networks)`).
    async fn list_labeled(&self) -> Result<(Vec<String>, Vec<String>)>;
    /// Execute `argv` in the container, inheriting stdio, returning the exit code.
    async fn exec(
        &self,
        name: &str,
        workdir: &str,
        user: &str,
        argv: &[String],
    ) -> Result<ExecResult>;
    /// Execute `argv`, capturing combined stdout+stderr (short control commands).
    async fn exec_capture(
        &self,
        name: &str,
        workdir: &str,
        user: &str,
        argv: &[String],
    ) -> Result<(ExecResult, String)>;
    /// Execute `argv` with `stdin` piped in (so multi-line payloads avoid shell
    /// quoting), capturing stdout. Used by the structured file tools.
    async fn exec_stdin(
        &self,
        name: &str,
        workdir: &str,
        user: &str,
        argv: &[String],
        stdin: &str,
    ) -> Result<(ExecResult, String)>;
    /// Execute a shell command, streaming combined stdout+stderr chunks to
    /// `chunks` as they arrive, while also accumulating the full output. The
    /// command runs in its own process group; on `cancel` or timeout the group
    /// is killed (no lingering children). Returns (exit, accumulated output).
    #[allow(clippy::too_many_arguments)]
    async fn exec_stream(
        &self,
        name: &str,
        workdir: &str,
        user: &str,
        command: &str,
        timeout_secs: u64,
        cancel: tokio_util::sync::CancellationToken,
        chunks: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Result<(ExecResult, String)>;
    /// Execute interactively (`-it`), inheriting the terminal.
    async fn exec_interactive(
        &self,
        name: &str,
        workdir: &str,
        user: &str,
        argv: &[String],
    ) -> Result<ExecResult>;
}

/// Existence/run state of a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerState {
    Absent,
    Running,
    Stopped,
}

/// The production implementation, backed by the bollard Docker API.
///
/// The client is connected lazily on first use and cached, so `new()` is
/// infallible and a down daemon surfaces as an operation error (more useful
/// than a construction error). `Clone` shares the same connection.
#[derive(Default, Clone)]
pub struct CliDocker {
    client: Arc<OnceCell<Docker>>,
}

impl CliDocker {
    pub fn new() -> Self {
        Self::default()
    }

    /// The connected bollard client, connecting on first use.
    async fn client(&self) -> Result<&Docker> {
        self.client
            .get_or_try_init(|| async {
                Docker::connect_with_defaults()
                    .context("connect to Docker daemon (is it installed and running?)")
            })
            .await
    }
}

/// A `Stdio` pointing at our own stderr, so a child's stdout is merged into our
/// stderr (where build progress belongs) instead of leaking onto our stdout and
/// polluting the output of the command we're about to run. Falls back to
/// discarding rather than ever risking stdout contamination.
fn stderr_stdio() -> Stdio {
    use std::os::fd::AsFd;
    std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null())
}

/// `Some(s)` if `s` is non-empty, else `None` — for bollard's `Option` fields.
fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

/// Split an image reference into `(repo, tag)`, defaulting to `latest`. The tag
/// is the part after the last `:` *only* when that `:` follows the final `/`
/// (so a registry `host:port/repo` is not mistaken for a tag).
fn split_ref(image: &str) -> (&str, &str) {
    let last_segment = image.rsplit_once('/').map_or(image, |(_, seg)| seg);
    if last_segment.contains(':') {
        let idx = image.rfind(':').expect("segment contains ':'");
        (&image[..idx], &image[idx + 1..])
    } else {
        (image, "latest")
    }
}

/// Parse a docker-style memory string (`512m`, `2g`, `1024`) into bytes
/// (powers of 1024, matching the docker CLI). Returns `None` if unparseable.
fn parse_memory(s: &str) -> Option<i64> {
    let s = s.trim();
    let last = s.chars().last()?;
    let (num, mult): (&str, i64) = match last.to_ascii_lowercase() {
        'b' => (&s[..s.len() - 1], 1),
        'k' => (&s[..s.len() - 1], 1024),
        'm' => (&s[..s.len() - 1], 1024 * 1024),
        'g' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        c if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    num.trim()
        .parse::<f64>()
        .ok()
        .map(|n| (n * mult as f64) as i64)
}

/// Map a [`ContainerSpec`] to bollard's create body + create options.
fn build_create(
    spec: &ContainerSpec,
) -> (
    ContainerCreateBody,
    bollard::query_parameters::CreateContainerOptions,
) {
    let mut host = HostConfig::default();
    if !spec.mounts.is_empty() {
        host.binds = Some(spec.mounts.iter().map(BindMount::to_arg).collect());
    }
    if let Some(mem) = spec.memory.as_deref() {
        host.memory = parse_memory(mem);
    }
    if let Some(cpus) = spec.cpus {
        host.nano_cpus = Some((cpus * 1e9) as i64);
    }
    if let Some(pids) = spec.pids_limit {
        host.pids_limit = Some(pids as i64);
    }
    if !spec.cap_drop.is_empty() {
        host.cap_drop = Some(spec.cap_drop.clone());
    }
    if !spec.cap_add.is_empty() {
        host.cap_add = Some(spec.cap_add.clone());
    }
    if !spec.dns.is_empty() {
        host.dns = Some(spec.dns.clone());
    }
    if !spec.extra_hosts.is_empty() {
        host.extra_hosts = Some(spec.extra_hosts.clone());
    }
    if !spec.security_opt.is_empty() {
        host.security_opt = Some(spec.security_opt.clone());
    }
    if !spec.sysctls.is_empty() {
        host.sysctls = Some(spec.sysctls.iter().cloned().collect());
    }

    // Networking: `network_mode` covers both named networks and `container:<id>`
    // namespace-sharing (the gateway). A static IP requires an endpoint config.
    let mut networking_config = None;
    if let Some(net) = &spec.network {
        host.network_mode = Some(net.clone());
        if let Some(ip) = &spec.ip {
            let endpoint = EndpointSettings {
                ipam_config: Some(EndpointIpamConfig {
                    ipv4_address: Some(ip.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            networking_config = Some(NetworkingConfig {
                endpoints_config: Some(HashMap::from([(net.clone(), endpoint)])),
            });
        }
    }

    // `tail -f /dev/null` keeps a container alive portably (works on the debian
    // default image and minimal busybox/alpine images alike).
    let cmd = spec.keep_alive.clone().unwrap_or_else(|| {
        vec![
            "tail".to_string(),
            "-f".to_string(),
            "/dev/null".to_string(),
        ]
    });

    let config = ContainerCreateBody {
        image: Some(spec.image.clone()),
        cmd: Some(cmd),
        working_dir: non_empty(&spec.workdir),
        user: spec.user.clone(),
        env: (!spec.env.is_empty())
            .then(|| spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect()),
        entrypoint: spec.entrypoint.as_ref().map(|e| vec![e.clone()]),
        // Labels: `cowboy=1` makes cowboy-managed containers discoverable for
        // teardown; `cowboy.version` records the binary version that created the
        // container so an upgraded binary recreates (rather than silently reuses)
        // a container from an older version.
        labels: Some(HashMap::from([
            ("cowboy".to_string(), "1".to_string()),
            (
                "cowboy.version".to_string(),
                env!("CARGO_PKG_VERSION").to_string(),
            ),
        ])),
        host_config: Some(host),
        networking_config,
        ..Default::default()
    };
    let opts = CreateContainerOptionsBuilder::new()
        .name(&spec.name)
        .build();
    (config, opts)
}

#[async_trait]
impl DockerCli for CliDocker {
    async fn image_exists(&self, image: &str) -> Result<bool> {
        let docker = self.client().await?;
        match docker.inspect_image(image).await {
            Ok(_) => Ok(true),
            Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(false),
            Err(e) => Err(e).context("docker image inspect"),
        }
    }

    async fn build_image(&self, dockerfile: &Path, context: &Path, tag: &str) -> Result<()> {
        // Shell out: the CLI tars the build context and applies `.dockerignore`
        // for us, and passes everything as argv (no quoting fragility).
        let status = Command::new("docker")
            .args(["build", "-t", tag, "-f"])
            .arg(dockerfile)
            .arg(context)
            // Build progress is diagnostic: keep it off our stdout so it can't
            // pollute a subsequent command's captured output.
            .stdout(stderr_stdio())
            .status()
            .await
            .context("docker build")?;
        if !status.success() {
            bail!("docker build failed for {tag} ({status})");
        }
        Ok(())
    }

    async fn pull_image(&self, image: &str) -> Result<()> {
        let docker = self.client().await?;
        let (repo, tag) = split_ref(image);
        let opts = CreateImageOptionsBuilder::new()
            .from_image(repo)
            .tag(tag)
            .build();
        let mut stream = docker.create_image(Some(opts), None, None);
        while let Some(item) = stream.next().await {
            let info = item.context("docker pull")?;
            // Print milestone lines (skip the per-layer progress ticks, which
            // carry a `progress_detail`) to stderr, where pull progress belongs.
            if info.progress_detail.is_none() {
                if let Some(s) = info.status {
                    eprintln!("{s}");
                }
            }
        }
        Ok(())
    }

    async fn container_state(&self, name: &str) -> Result<ContainerState> {
        let docker = self.client().await?;
        match docker.inspect_container(name, None).await {
            Ok(info) => {
                let running = info.state.and_then(|s| s.running).unwrap_or(false);
                Ok(if running {
                    ContainerState::Running
                } else {
                    ContainerState::Stopped
                })
            }
            Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(ContainerState::Absent),
            Err(e) => Err(e).context("docker inspect"),
        }
    }

    async fn container_label(&self, name: &str, key: &str) -> Result<Option<String>> {
        let docker = self.client().await?;
        match docker.inspect_container(name, None).await {
            Ok(info) => Ok(info
                .config
                .and_then(|c| c.labels)
                .and_then(|l| l.get(key).cloned())),
            Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(None),
            Err(e) => Err(e).context("docker inspect (label)"),
        }
    }

    async fn run_detached(&self, spec: &ContainerSpec) -> Result<()> {
        let docker = self.client().await?;
        let (config, opts) = build_create(spec);
        docker
            .create_container(Some(opts), config)
            .await
            .context("docker create")?;
        docker
            .start_container(
                &spec.name,
                None::<bollard::query_parameters::StartContainerOptions>,
            )
            .await
            .context("docker start")?;
        Ok(())
    }

    async fn run_oneshot(&self, spec: &ContainerSpec) -> Result<ExecResult> {
        let docker = self.client().await?;
        let (config, opts) = build_create(spec);
        docker
            .create_container(Some(opts), config)
            .await
            .context("docker create (oneshot)")?;
        docker
            .start_container(
                &spec.name,
                None::<bollard::query_parameters::StartContainerOptions>,
            )
            .await
            .context("docker start (oneshot)")?;
        // Wait for exit. bollard surfaces a non-zero exit as a typed error
        // carrying the code; both paths give us the code we return.
        let mut wait = docker.wait_container(
            &spec.name,
            None::<bollard::query_parameters::WaitContainerOptions>,
        );
        let mut code: i64 = 0;
        while let Some(item) = wait.next().await {
            match item {
                Ok(resp) => code = resp.status_code,
                Err(BollardError::DockerContainerWaitError { code: c, .. }) => code = c,
                Err(e) => return Err(e).context("docker wait (oneshot)"),
            }
        }
        // `--rm` equivalent: remove the finished container (best-effort).
        let opts = RemoveContainerOptionsBuilder::new().force(true).build();
        let _ = docker.remove_container(&spec.name, Some(opts)).await;
        Ok(ExecResult {
            exit_code: code as i32,
        })
    }

    async fn network_exists(&self, name: &str) -> Result<bool> {
        let docker = self.client().await?;
        match docker
            .inspect_network(
                name,
                None::<bollard::query_parameters::InspectNetworkOptions>,
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(false),
            Err(e) => Err(e).context("docker network inspect"),
        }
    }

    async fn create_network(&self, spec: &NetworkSpec) -> Result<()> {
        let docker = self.client().await?;
        let ipam = (spec.subnet.is_some() || spec.gateway.is_some()).then(|| Ipam {
            config: Some(vec![IpamConfig {
                subnet: spec.subnet.clone(),
                gateway: spec.gateway.clone(),
                ..Default::default()
            }]),
            ..Default::default()
        });
        let req = NetworkCreateRequest {
            name: spec.name.clone(),
            internal: Some(spec.internal),
            ipam,
            labels: Some(HashMap::from([("cowboy".to_string(), "1".to_string())])),
            ..Default::default()
        };
        docker
            .create_network(req)
            .await
            .context("docker network create")?;
        Ok(())
    }

    async fn connect_network(&self, network: &str, container: &str) -> Result<()> {
        let docker = self.client().await?;
        docker
            .connect_network(
                network,
                NetworkConnectRequest {
                    container: container.to_string(),
                    ..Default::default()
                },
            )
            .await
            .context("docker network connect")?;
        Ok(())
    }

    async fn remove_network(&self, name: &str) -> Result<()> {
        let docker = self.client().await?;
        let _ = docker.remove_network(name).await; // ignore "not found"
        Ok(())
    }

    async fn list_labeled(&self) -> Result<(Vec<String>, Vec<String>)> {
        let docker = self.client().await?;
        let filters: HashMap<String, Vec<String>> =
            HashMap::from([("label".to_string(), vec!["cowboy=1".to_string()])]);
        let containers = docker
            .list_containers(Some(
                ListContainersOptionsBuilder::new()
                    .all(true)
                    .filters(&filters)
                    .build(),
            ))
            .await
            .context("docker ps")?
            .into_iter()
            .filter_map(|c| c.id)
            .collect();
        let networks = docker
            .list_networks(Some(
                ListNetworksOptionsBuilder::new().filters(&filters).build(),
            ))
            .await
            .context("docker network ls")?
            .into_iter()
            .filter_map(|n| n.id)
            .collect();
        Ok((containers, networks))
    }

    async fn start(&self, name: &str) -> Result<()> {
        let docker = self.client().await?;
        docker
            .start_container(
                name,
                None::<bollard::query_parameters::StartContainerOptions>,
            )
            .await
            .context("docker start")?;
        Ok(())
    }

    async fn stop(&self, name: &str) -> Result<()> {
        let docker = self.client().await?;
        let _ = docker
            .stop_container(
                name,
                None::<bollard::query_parameters::StopContainerOptions>,
            )
            .await; // best-effort (already stopped / absent)
        Ok(())
    }

    async fn remove(&self, name: &str, force: bool) -> Result<()> {
        let docker = self.client().await?;
        let opts = RemoveContainerOptionsBuilder::new().force(force).build();
        let _ = docker.remove_container(name, Some(opts)).await; // ignore "not found"
        Ok(())
    }

    async fn exec(
        &self,
        name: &str,
        workdir: &str,
        user: &str,
        argv: &[String],
    ) -> Result<ExecResult> {
        let docker = self.client().await?;
        let exec = docker
            .create_exec(
                name,
                CreateExecOptions {
                    cmd: Some(argv.to_vec()),
                    working_dir: non_empty(workdir),
                    user: non_empty(user),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .context("docker exec (create)")?;
        if let StartExecResults::Attached { mut output, .. } = docker
            .start_exec(&exec.id, None)
            .await
            .context("docker exec")?
        {
            use tokio::io::AsyncWriteExt;
            let mut out = tokio::io::stdout();
            let mut err = tokio::io::stderr();
            while let Some(frame) = output.next().await {
                match frame.context("docker exec (stream)")? {
                    LogOutput::StdErr { message } => {
                        err.write_all(&message).await.ok();
                    }
                    other => {
                        out.write_all(&other.into_bytes()).await.ok();
                    }
                }
            }
            out.flush().await.ok();
            err.flush().await.ok();
        }
        Ok(ExecResult {
            exit_code: exec_exit_code(docker, &exec.id).await,
        })
    }

    async fn exec_capture(
        &self,
        name: &str,
        workdir: &str,
        user: &str,
        argv: &[String],
    ) -> Result<(ExecResult, String)> {
        let docker = self.client().await?;
        let exec = docker
            .create_exec(
                name,
                CreateExecOptions {
                    cmd: Some(argv.to_vec()),
                    working_dir: non_empty(workdir),
                    user: non_empty(user),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .context("docker exec (create)")?;
        let mut combined = String::new();
        if let StartExecResults::Attached { mut output, .. } = docker
            .start_exec(&exec.id, None)
            .await
            .context("docker exec (capture)")?
        {
            while let Some(frame) = output.next().await {
                let bytes = frame.context("docker exec (capture)")?.into_bytes();
                combined.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
        Ok((
            ExecResult {
                exit_code: exec_exit_code(docker, &exec.id).await,
            },
            combined,
        ))
    }

    async fn exec_stdin(
        &self,
        name: &str,
        workdir: &str,
        user: &str,
        argv: &[String],
        stdin: &str,
    ) -> Result<(ExecResult, String)> {
        let docker = self.client().await?;
        let exec = docker
            .create_exec(
                name,
                CreateExecOptions {
                    cmd: Some(argv.to_vec()),
                    working_dir: non_empty(workdir),
                    user: non_empty(user),
                    attach_stdin: Some(true),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .context("docker exec (create)")?;
        let mut combined = String::new();
        if let StartExecResults::Attached {
            mut output,
            mut input,
        } = docker
            .start_exec(&exec.id, None)
            .await
            .context("docker exec (stdin)")?
        {
            use tokio::io::AsyncWriteExt;
            // Write stdin on a separate task so a command that emits output
            // before consuming all of stdin can't deadlock the single
            // multiplexed connection.
            let payload = stdin.as_bytes().to_vec();
            let writer = tokio::spawn(async move {
                let _ = input.write_all(&payload).await;
                let _ = input.shutdown().await;
            });
            while let Some(frame) = output.next().await {
                let bytes = frame.context("docker exec (stdin)")?.into_bytes();
                combined.push_str(&String::from_utf8_lossy(&bytes));
            }
            let _ = writer.await;
        }
        Ok((
            ExecResult {
                exit_code: exec_exit_code(docker, &exec.id).await,
            },
            combined,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn exec_stream(
        &self,
        name: &str,
        workdir: &str,
        user: &str,
        command: &str,
        timeout_secs: u64,
        cancel: tokio_util::sync::CancellationToken,
        chunks: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Result<(ExecResult, String)> {
        let docker = self.client().await?;

        // Run the command in its own process group, recording the leader pid so
        // we can signal the whole group on cancel/timeout. The command is passed
        // via env to avoid shell quoting. No PTY: over a plain pipe, tools like
        // mise/cargo/docker detect "not a TTY" and emit plain, streamable log
        // lines instead of cursor-movement progress we'd have to emulate. The
        // wrapper's `2>&1` merges stderr into stdout.
        //
        // Each exec gets a UNIQUE tag (#1): the pidfile name is per-exec, so
        // concurrent execs don't overwrite each other's recorded pgid (which would
        // make a cancel/timeout kill the wrong group and leak the real command).
        // The tag is also exported as an env var (#2) inherited by every
        // descendant — even ones that re-`setsid` into their own process group and
        // so escape the recorded pgid (e.g. a `mise` hook) — so the kill can sweep
        // them by marker as a backstop to the group signal.
        let tag = next_exec_tag();
        let pidfile = format!("/tmp/cowboy-exec-{tag}.pgid");
        // `setsid` puts the command in its own group (leader pid recorded in the
        // pidfile). We don't `exec` the inner shell so the wrapper can `rm` the
        // pidfile on normal exit (the kill path removes it otherwise); killing is
        // by process group, so the extra shell layer doesn't affect signalling.
        let wrapper = format!(
            "setsid sh -c 'echo $$ > {pidfile}; sh -c \"$COWBOY_CMD\"; r=$?; rm -f {pidfile}; exit $r' 2>&1"
        );
        let exec = docker
            .create_exec(
                name,
                CreateExecOptions {
                    cmd: Some(vec!["sh".to_string(), "-c".to_string(), wrapper]),
                    env: Some(vec![
                        format!("COWBOY_CMD={command}"),
                        format!("COWBOY_EXEC_TAG={tag}"),
                    ]),
                    working_dir: non_empty(workdir),
                    user: non_empty(user),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .context("docker exec (create stream)")?;

        let mut output = match docker
            .start_exec(&exec.id, None)
            .await
            .context("starting streaming docker exec")?
        {
            StartExecResults::Attached { output, .. } => output,
            StartExecResults::Detached => bail!("docker exec started detached"),
        };

        let mut accumulated = String::new();
        // Split the byte stream ourselves: `\n` (and `\r\n`) commits a line; a
        // bare `\r` is a single-line progress overwrite (transient). `line_start`
        // marks where the current line begins in `accumulated` so transient
        // updates replace it in place. (UTF-8 multibyte never contains
        // 0x0A/0x0D, so splitting on those bytes is safe.)
        let mut line: Vec<u8> = Vec::new();
        let mut line_start = 0usize;
        let mut pending_cr = false;

        let timeout = if timeout_secs == 0 {
            std::time::Duration::from_secs(86_400)
        } else {
            std::time::Duration::from_secs(timeout_secs)
        };
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);

        let mut interrupted: Option<&str> = None;
        loop {
            tokio::select! {
                frame = output.next() => match frame {
                    None => break, // EOF
                    Some(Err(_)) => break, // stream error
                    Some(Ok(f)) => {
                        for &b in f.into_bytes().iter() {
                            if pending_cr {
                                pending_cr = false;
                                if b == b'\n' {
                                    commit_line(&mut accumulated, &mut line_start, &mut line, &chunks);
                                    continue;
                                }
                                // bare `\r`: overwrite the line so far, then this
                                // byte starts fresh content on the same line.
                                transient_line(&mut accumulated, line_start, &mut line, &chunks);
                            }
                            match b {
                                b'\n' => {
                                    commit_line(&mut accumulated, &mut line_start, &mut line, &chunks)
                                }
                                b'\r' => pending_cr = true,
                                _ => line.push(b),
                            }
                        }
                    }
                },
                _ = cancel.cancelled() => { interrupted = Some("cancelled"); break; }
                _ = &mut deadline => { interrupted = Some("timed out"); break; }
            }
        }
        // Flush any trailing partial line (no final newline) as transient.
        transient_line(&mut accumulated, line_start, &mut line, &chunks);

        if let Some(why) = interrupted {
            // Kill the command in the container, then drop the local stream.
            let _ = self
                .exec_capture(
                    name,
                    "",
                    "",
                    &["sh".into(), "-c".into(), kill_exec_script(&pidfile, &tag)],
                )
                .await;
            drop(output);
            let note = format!("[command {why}]");
            accumulated.push_str(&note);
            let _ = chunks.send(format!("{note}\n"));
            return Ok((
                ExecResult {
                    exit_code: if why == "timed out" { 124 } else { 130 },
                },
                accumulated,
            ));
        }

        Ok((
            ExecResult {
                exit_code: exec_exit_code(docker, &exec.id).await,
            },
            accumulated,
        ))
    }

    async fn exec_interactive(
        &self,
        name: &str,
        workdir: &str,
        user: &str,
        argv: &[String],
    ) -> Result<ExecResult> {
        // Shell out: the CLI puts the host terminal into raw mode, allocates a
        // PTY, and forwards window-resize events for us. argv is passed as a
        // list (no quoting fragility).
        let mut cmd = Command::new("docker");
        cmd.arg("exec");
        cmd.arg("-it");
        push_exec_flags(&mut cmd, workdir, user);
        cmd.arg(name);
        cmd.args(argv);
        let status = cmd.status().await.context("docker exec -it")?;
        Ok(ExecResult {
            exit_code: status.code().unwrap_or(-1),
        })
    }
}

/// Inspect a finished exec for its exit code (`-1` if unavailable).
async fn exec_exit_code(docker: &Docker, exec_id: &str) -> i32 {
    docker
        .inspect_exec(exec_id)
        .await
        .ok()
        .and_then(|r| r.exit_code)
        .map(|c| c as i32)
        .unwrap_or(-1)
}

/// Push `-w <workdir>` and (if non-empty) `--user <user>` onto a `docker exec`.
fn push_exec_flags(cmd: &mut Command, workdir: &str, user: &str) {
    cmd.args(["-w", workdir]);
    if !user.is_empty() {
        cmd.args(["--user", user]);
    }
}

/// Commit the current line (`\n` reached): record it in `output` with a
/// newline and send it as a *committed* chunk (trailing newline — the UI
/// appends it). `line_start` advances past it.
fn commit_line(
    output: &mut String,
    line_start: &mut usize,
    buf: &mut Vec<u8>,
    tx: &tokio::sync::mpsc::UnboundedSender<String>,
) {
    let text = String::from_utf8_lossy(buf);
    output.truncate(*line_start);
    output.push_str(&text);
    output.push('\n');
    let _ = tx.send(format!("{text}\n"));
    *line_start = output.len();
    buf.clear();
}

/// Flush the current line as *transient* (a bare `\r` overwrite, e.g. a progress
/// bar): replace it in `output` (no newline) and send it without a trailing
/// newline so the UI overwrites the last line in place instead of appending.
fn transient_line(
    output: &mut String,
    line_start: usize,
    buf: &mut Vec<u8>,
    tx: &tokio::sync::mpsc::UnboundedSender<String>,
) {
    if buf.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(buf);
    output.truncate(line_start);
    output.push_str(&text);
    let _ = tx.send(text.into_owned());
    buf.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_mount_args() {
        assert_eq!(BindMount::rw("/a", "/b").to_arg(), "/a:/b");
        assert_eq!(BindMount::ro("/a", "/b").to_arg(), "/a:/b:ro");
    }

    #[test]
    fn exec_tags_are_unique() {
        // Concurrent execs must not share a kill target (the shared-pidfile bug).
        let a = next_exec_tag();
        let b = next_exec_tag();
        assert_ne!(a, b);
        assert!(a.starts_with(&format!("{}-", std::process::id())));
    }

    #[test]
    fn kill_script_targets_group_and_tag_then_cleans_up() {
        let script = kill_exec_script("/tmp/cowboy-exec-42-7.pgid", "42-7");
        // Reads the per-exec pidfile (not a shared one) and signals its group.
        assert!(script.contains("cat /tmp/cowboy-exec-42-7.pgid"));
        assert!(script.contains("kill -TERM -\"$p\""));
        assert!(script.contains("kill -KILL -\"$p\""));
        // Sweeps /proc for the escapee marker, TERM then KILL.
        assert!(script.contains("COWBOY_EXEC_TAG=42-7"));
        assert!(script.contains("kill -TERM \"$(basename"));
        assert!(script.contains("kill -KILL \"$(basename"));
        // Cleans up the pidfile so /tmp doesn't accumulate.
        assert!(script.contains("rm -f /tmp/cowboy-exec-42-7.pgid"));
    }

    #[test]
    fn split_ref_extracts_tag_not_registry_port() {
        assert_eq!(split_ref("alpine"), ("alpine", "latest"));
        assert_eq!(split_ref("alpine:3.20"), ("alpine", "3.20"));
        assert_eq!(
            split_ref("ghcr.io/koshea/cowboy/gateway:0.1.0"),
            ("ghcr.io/koshea/cowboy/gateway", "0.1.0")
        );
        // registry host:port with no tag → repo unchanged, default tag.
        assert_eq!(
            split_ref("localhost:5000/img"),
            ("localhost:5000/img", "latest")
        );
        assert_eq!(
            split_ref("localhost:5000/img:v2"),
            ("localhost:5000/img", "v2")
        );
    }

    #[test]
    fn parse_memory_handles_suffixes() {
        assert_eq!(parse_memory("1024"), Some(1024));
        assert_eq!(parse_memory("512m"), Some(512 * 1024 * 1024));
        assert_eq!(parse_memory("2g"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_memory("8G"), Some(8 * 1024 * 1024 * 1024));
        assert_eq!(parse_memory("nonsense"), None);
        assert_eq!(parse_memory(""), None);
    }

    #[test]
    fn build_create_maps_spec_to_host_config() {
        let spec = ContainerSpec {
            name: "c".into(),
            image: "img:1".into(),
            workdir: "/w".into(),
            mounts: vec![BindMount::ro("/h", "/c")],
            env: vec![("K".into(), "V".into())],
            network: Some("net".into()),
            ip: Some("10.0.0.5".into()),
            memory: Some("256m".into()),
            cpus: Some(2.0),
            cap_drop: vec!["ALL".into()],
            pids_limit: Some(4096),
            ..Default::default()
        };
        let (config, _opts) = build_create(&spec);
        let host = config.host_config.unwrap();
        assert_eq!(host.binds, Some(vec!["/h:/c:ro".to_string()]));
        assert_eq!(host.memory, Some(256 * 1024 * 1024));
        assert_eq!(host.nano_cpus, Some(2_000_000_000));
        assert_eq!(host.pids_limit, Some(4096));
        assert_eq!(host.network_mode.as_deref(), Some("net"));
        assert_eq!(config.image.as_deref(), Some("img:1"));
        assert_eq!(config.working_dir.as_deref(), Some("/w"));
        assert_eq!(config.env, Some(vec!["K=V".to_string()]));
        // Labels: discovery marker + the creating cowboy version (so an upgraded
        // binary can detect and recreate a stale-version container).
        let labels = config.labels.unwrap();
        assert_eq!(labels.get("cowboy").map(String::as_str), Some("1"));
        assert_eq!(
            labels.get("cowboy.version").map(String::as_str),
            Some(env!("CARGO_PKG_VERSION"))
        );
        // static IP recorded on the endpoint config for `net`.
        let ep = config
            .networking_config
            .unwrap()
            .endpoints_config
            .unwrap()
            .remove("net")
            .unwrap();
        assert_eq!(
            ep.ipam_config.unwrap().ipv4_address.as_deref(),
            Some("10.0.0.5")
        );
    }
}
