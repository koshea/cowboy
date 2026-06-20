//! Docker control.
//!
//! For the MVP we shell out to the `docker` CLI, behind the [`DockerCli`] trait
//! so the implementation can be swapped for `bollard` later without touching
//! callers. The trait is mockable (`mockall`) for unit tests.

use std::path::Path;
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use tokio::process::Command;

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

/// Docker operations cowboy needs. Shell-out today, `bollard` tomorrow.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait DockerCli: Send + Sync {
    async fn image_exists(&self, image: &str) -> Result<bool>;
    async fn build_image(&self, dockerfile: &Path, context: &Path, tag: &str) -> Result<()>;
    async fn pull_image(&self, image: &str) -> Result<()>;
    async fn container_state(&self, name: &str) -> Result<ContainerState>;
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

/// The default implementation that shells out to the `docker` binary.
#[derive(Debug, Default, Clone)]
pub struct CliDocker;

impl CliDocker {
    pub fn new() -> Self {
        Self
    }
}

async fn run_quiet(args: &[&str]) -> Result<std::process::Output> {
    Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .context("failed to invoke `docker` (is it installed and running?)")
}

/// A `Stdio` pointing at our own stderr, so a child's stdout is merged into our
/// stderr (where build/pull progress belongs) instead of leaking onto our stdout
/// and polluting the output of the command we're about to run. Falls back to
/// discarding rather than ever risking stdout contamination.
fn stderr_stdio() -> Stdio {
    use std::os::fd::AsFd;
    std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null())
}

/// Append the common `docker run` flags for a [`ContainerSpec`] to `args`
/// (everything after `run [-d|--rm]`). Shared by detached and one-shot runs.
fn render_run_args(args: &mut Vec<String>, spec: &ContainerSpec) {
    if !spec.name.is_empty() {
        args.push("--name".into());
        args.push(spec.name.clone());
    }
    if !spec.workdir.is_empty() {
        args.push("-w".into());
        args.push(spec.workdir.clone());
    }
    if let Some(user) = &spec.user {
        args.push("--user".into());
        args.push(user.clone());
    }
    for opt in &spec.security_opt {
        args.push("--security-opt".into());
        args.push(opt.clone());
    }
    if let Some(pids) = spec.pids_limit {
        args.push("--pids-limit".into());
        args.push(pids.to_string());
    }
    if let Some(net) = &spec.network {
        args.push("--network".into());
        args.push(net.clone());
    }
    if let Some(ip) = &spec.ip {
        args.push("--ip".into());
        args.push(ip.clone());
    }
    if let Some(mem) = &spec.memory {
        args.push("--memory".into());
        args.push(mem.clone());
    }
    if let Some(cpus) = spec.cpus {
        args.push("--cpus".into());
        args.push(cpus.to_string());
    }
    for cap in &spec.cap_drop {
        args.push("--cap-drop".into());
        args.push(cap.clone());
    }
    for cap in &spec.cap_add {
        args.push("--cap-add".into());
        args.push(cap.clone());
    }
    for (k, v) in &spec.sysctls {
        args.push("--sysctl".into());
        args.push(format!("{k}={v}"));
    }
    for d in &spec.dns {
        args.push("--dns".into());
        args.push(d.clone());
    }
    for h in &spec.extra_hosts {
        args.push("--add-host".into());
        args.push(h.clone());
    }
    if let Some(ep) = &spec.entrypoint {
        args.push("--entrypoint".into());
        args.push(ep.clone());
    }
    for m in &spec.mounts {
        args.push("-v".into());
        args.push(m.to_arg());
    }
    for (k, v) in &spec.env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    // Label so cowboy-managed containers are discoverable for teardown.
    args.push("--label".into());
    args.push("cowboy=1".into());
    args.push(spec.image.clone());
    match &spec.keep_alive {
        Some(cmd) => args.extend(cmd.iter().cloned()),
        // `tail -f /dev/null` keeps a container alive portably (works on both
        // the debian default image and minimal busybox/alpine images).
        None => args.extend([
            "tail".to_string(),
            "-f".to_string(),
            "/dev/null".to_string(),
        ]),
    }
}

#[async_trait]
impl DockerCli for CliDocker {
    async fn image_exists(&self, image: &str) -> Result<bool> {
        let out = run_quiet(&["image", "inspect", image]).await?;
        Ok(out.status.success())
    }

    async fn build_image(&self, dockerfile: &Path, context: &Path, tag: &str) -> Result<()> {
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
        let status = Command::new("docker")
            .args(["pull", image])
            // `docker pull` writes its progress to stdout; route it to stderr so
            // it never lands in the output of the command we're about to run.
            .stdout(stderr_stdio())
            .status()
            .await
            .context("docker pull")?;
        if !status.success() {
            bail!("docker pull failed for {image} ({status})");
        }
        Ok(())
    }

    async fn container_state(&self, name: &str) -> Result<ContainerState> {
        let out = run_quiet(&["inspect", "-f", "{{.State.Running}}", name]).await?;
        if !out.status.success() {
            return Ok(ContainerState::Absent);
        }
        let running = String::from_utf8_lossy(&out.stdout).trim() == "true";
        Ok(if running {
            ContainerState::Running
        } else {
            ContainerState::Stopped
        })
    }

    async fn run_detached(&self, spec: &ContainerSpec) -> Result<()> {
        let mut args = vec!["run".to_string(), "-d".to_string()];
        render_run_args(&mut args, spec);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run_quiet(&argv).await?;
        if !out.status.success() {
            bail!(
                "docker run failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    async fn run_oneshot(&self, spec: &ContainerSpec) -> Result<ExecResult> {
        let mut args = vec!["run".to_string(), "--rm".to_string()];
        render_run_args(&mut args, spec);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run_quiet(&argv).await?;
        if !out.status.success() {
            bail!(
                "docker run (oneshot) failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(ExecResult {
            exit_code: out.status.code().unwrap_or(-1),
        })
    }

    async fn network_exists(&self, name: &str) -> Result<bool> {
        let out = run_quiet(&["network", "inspect", name]).await?;
        Ok(out.status.success())
    }

    async fn create_network(&self, spec: &NetworkSpec) -> Result<()> {
        let mut args = vec!["network".to_string(), "create".to_string()];
        if spec.internal {
            args.push("--internal".into());
        }
        if let Some(subnet) = &spec.subnet {
            args.push("--subnet".into());
            args.push(subnet.clone());
        }
        if let Some(gw) = &spec.gateway {
            args.push("--gateway".into());
            args.push(gw.clone());
        }
        args.push("--label".into());
        args.push("cowboy=1".into());
        args.push(spec.name.clone());
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run_quiet(&argv).await?;
        if !out.status.success() {
            bail!(
                "docker network create failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    async fn connect_network(&self, network: &str, container: &str) -> Result<()> {
        let out = run_quiet(&["network", "connect", network, container]).await?;
        if !out.status.success() {
            bail!(
                "docker network connect failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    async fn remove_network(&self, name: &str) -> Result<()> {
        let _ = run_quiet(&["network", "rm", name]).await?; // ignore "not found"
        Ok(())
    }

    async fn list_labeled(&self) -> Result<(Vec<String>, Vec<String>)> {
        let lines = |out: std::process::Output| -> Vec<String> {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::to_string)
                .filter(|s| !s.is_empty())
                .collect()
        };
        let containers = run_quiet(&["ps", "-aq", "--filter", "label=cowboy=1"]).await?;
        let networks = run_quiet(&["network", "ls", "-q", "--filter", "label=cowboy=1"]).await?;
        Ok((lines(containers), lines(networks)))
    }

    async fn start(&self, name: &str) -> Result<()> {
        let out = run_quiet(&["start", name]).await?;
        if !out.status.success() {
            bail!(
                "docker start failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    async fn stop(&self, name: &str) -> Result<()> {
        let _ = run_quiet(&["stop", name]).await?;
        Ok(())
    }

    async fn remove(&self, name: &str, force: bool) -> Result<()> {
        let mut args = vec!["rm"];
        if force {
            args.push("-f");
        }
        args.push(name);
        let _ = run_quiet(&args).await?;
        Ok(())
    }

    async fn exec(
        &self,
        name: &str,
        workdir: &str,
        user: &str,
        argv: &[String],
    ) -> Result<ExecResult> {
        let mut cmd = Command::new("docker");
        cmd.arg("exec");
        push_exec_flags(&mut cmd, workdir, user);
        cmd.arg(name);
        cmd.args(argv);
        let status = cmd.status().await.context("docker exec")?;
        Ok(ExecResult {
            exit_code: status.code().unwrap_or(-1),
        })
    }

    async fn exec_capture(
        &self,
        name: &str,
        workdir: &str,
        user: &str,
        argv: &[String],
    ) -> Result<(ExecResult, String)> {
        let mut cmd = Command::new("docker");
        cmd.arg("exec");
        push_exec_flags(&mut cmd, workdir, user);
        cmd.arg(name);
        cmd.args(argv);
        cmd.kill_on_drop(true);
        let out = cmd.output().await.context("docker exec (capture)")?;
        let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.is_empty() {
            combined.push_str(&stderr);
        }
        Ok((
            ExecResult {
                exit_code: out.status.code().unwrap_or(-1),
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
        use tokio::io::AsyncWriteExt;
        let mut cmd = Command::new("docker");
        cmd.arg("exec").arg("-i");
        push_exec_flags(&mut cmd, workdir, user);
        cmd.arg(name);
        cmd.args(argv);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        let mut child = cmd.spawn().context("docker exec (stdin)")?;
        // The in-container helper reads all of stdin before emitting output, so
        // writing fully then awaiting completion does not deadlock.
        if let Some(mut si) = child.stdin.take() {
            si.write_all(stdin.as_bytes()).await?;
            si.shutdown().await.ok();
            drop(si);
        }
        let out = child
            .wait_with_output()
            .await
            .context("docker exec (stdin)")?;
        let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.is_empty() {
            combined.push_str(&stderr);
        }
        Ok((
            ExecResult {
                exit_code: out.status.code().unwrap_or(-1),
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
        use tokio::io::AsyncReadExt;

        // Run the command in its own process group, recording the leader pid so
        // we can signal the whole group on cancel/timeout. The command is passed
        // via env to avoid shell quoting.
        let pidfile = "/tmp/cowboy-exec.pgid";
        let wrapper =
            format!("setsid sh -c 'echo $$ > {pidfile}; exec sh -c \"$COWBOY_CMD\"' 2>&1");
        let mut cmd = Command::new("docker");
        cmd.arg("exec");
        // No `-t` (PTY): with a real terminal, tools like mise/cargo/docker draw
        // multi-line progress with cursor-movement escapes (cursor-up + reprint)
        // that we'd need a terminal emulator to render — they tiled down the
        // screen. Over a plain pipe those tools detect "not a TTY" and emit plain,
        // streamable log lines instead, which our line splitter handles.
        push_exec_flags(&mut cmd, workdir, user);
        cmd.args([
            "-e",
            &format!("COWBOY_CMD={command}"),
            name,
            "sh",
            "-c",
            &wrapper,
        ]);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null()); // merged into stdout by the wrapper's 2>&1
        cmd.kill_on_drop(true);

        let mut child = cmd.spawn().context("spawning streaming docker exec")?;
        let mut stdout = child.stdout.take().context("exec stdout")?;
        let mut output = String::new();
        // Split the byte stream ourselves: `\n` (and `\r\n`) commits a line; a
        // bare `\r` is a single-line progress overwrite (transient). `line_start`
        // marks where the current line begins in `output` so transient updates
        // replace it in place. (UTF-8 multibyte never contains 0x0A/0x0D, so
        // splitting on those bytes is safe.)
        let mut line: Vec<u8> = Vec::new();
        let mut line_start = 0usize;
        let mut pending_cr = false;
        let mut rbuf = [0u8; 8192];

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
                read = stdout.read(&mut rbuf) => match read {
                    Ok(0) | Err(_) => break, // EOF or read error
                    Ok(n) => {
                        for &b in &rbuf[..n] {
                            if pending_cr {
                                pending_cr = false;
                                if b == b'\n' {
                                    commit_line(&mut output, &mut line_start, &mut line, &chunks);
                                    continue;
                                }
                                // bare `\r`: overwrite the line so far, then this
                                // byte starts fresh content on the same line.
                                transient_line(&mut output, line_start, &mut line, &chunks);
                            }
                            match b {
                                b'\n' => {
                                    commit_line(&mut output, &mut line_start, &mut line, &chunks)
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
        transient_line(&mut output, line_start, &mut line, &chunks);

        if let Some(why) = interrupted {
            // Kill the in-container process group, then the local client.
            let kill = format!(
                "p=$(cat {pidfile} 2>/dev/null); [ -n \"$p\" ] && \
                 (kill -TERM -\"$p\" 2>/dev/null; sleep 1; kill -KILL -\"$p\" 2>/dev/null) || true"
            );
            let _ = run_quiet(&["exec", name, "sh", "-c", &kill]).await;
            let _ = child.start_kill();
            let note = format!("[command {why}]");
            output.push_str(&note);
            let _ = chunks.send(format!("{note}\n"));
            return Ok((
                ExecResult {
                    exit_code: if why == "timed out" { 124 } else { 130 },
                },
                output,
            ));
        }

        let status = child.wait().await.context("waiting for docker exec")?;
        Ok((
            ExecResult {
                exit_code: status.code().unwrap_or(-1),
            },
            output,
        ))
    }

    async fn exec_interactive(
        &self,
        name: &str,
        workdir: &str,
        user: &str,
        argv: &[String],
    ) -> Result<ExecResult> {
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
}
