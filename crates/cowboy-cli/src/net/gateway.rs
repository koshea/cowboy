//! Host-side bring-up of the sole-egress network gateway.
//!
//! Topology (per project, to avoid cross-project clashes):
//! ```text
//! agent container ──(cowboy-net)──► internet
//!   ▲ shares netns
//! gateway sidecar (--network container:<agent>, NET_ADMIN)
//!   installs nft REDIRECT in the shared netns → in-process proxy → allow/deny/ask
//! ```
//! The gateway runs as a **sidecar in the agent's network namespace** rather than
//! a separate routing hop. It applies an nft `nat output` REDIRECT (exempting its
//! own root-uid sockets) that forces all agent egress through its policy proxy.
//! The agent has its `NET_ADMIN`/`NET_RAW` dropped, so it can't undo the rules —
//! the only path out is the proxy, which fails closed. Co-locating in the agent's
//! netns means `SO_ORIGINAL_DST` works locally and there's no container-to-router
//! L3 forwarding (which Docker Desktop's gvisor backend does not support).

use std::path::PathBuf;

use anyhow::{Context, Result};
use cowboy_core::config::SecurityConfig;

use super::docker::{BindMount, ContainerSpec, DockerCli, NetworkSpec};

/// The gateway image, version-pinned to this binary (built from
/// `docker/gateway.Dockerfile` when developing, pulled from GHCR otherwise).
pub const GATEWAY_IMAGE: &str =
    concat!("ghcr.io/koshea/cowboy/gateway:", env!("CARGO_PKG_VERSION"));

/// Per-project docker object names derived from the project hash:
/// `(agent_net, gateway_container)`.
pub fn network_names(hash: u32) -> (String, String) {
    (
        format!("cowboy-net-{hash:08x}"),
        format!("cowboy-gw-{hash:08x}"),
    )
}

/// Per-project gateway networking parameters.
#[derive(Debug, Clone)]
pub struct GatewayNetwork {
    /// The agent's egress-capable network (the gateway sidecar shares its netns).
    pub agent_net: String,
    pub gateway_name: String,
    pub subnet: String,
    /// Host-side gateway IP of `agent_net`; the host binds its control server here
    /// on Linux (it owns the bridge). Unused as a bind address on Docker Desktop.
    pub bridge_gateway: String,
    policy_file: PathBuf,
    /// Address the gateway dials over TCP for `ask` decisions, from inside the
    /// container. On Linux this is the bridge gateway IP (the host owns it); on
    /// Docker Desktop (Mac/Windows) the host is a VM hop away, so the gateway
    /// dials `host.docker.internal` (mapped via `--add-host`).
    control_addr: String,
    /// Address the *host* binds its control server on. Same as `control_addr` on
    /// Linux; on Docker Desktop the host has no bridge interface, so it binds
    /// loopback (`127.0.0.1`) — reachable from the gateway via `control_addr`,
    /// and not LAN-exposed, preserving the "never `0.0.0.0`" invariant.
    control_bind_addr: String,
    /// Per-session token the gateway must present (the agent never sees it).
    control_token: String,
}

impl GatewayNetwork {
    /// Derive networking parameters for a project, keyed by a 32-bit hash so
    /// concurrent projects get non-overlapping subnets and distinct names.
    /// Persisted approvals under `root/.cowboy/approvals.json` are merged into
    /// the policy the gateway loads.
    pub fn for_project(
        hash: u32,
        security: &SecurityConfig,
        root: &std::path::Path,
    ) -> Result<Self> {
        let octet = (hash % 200 + 20) as u8; // 20..=219
        let subnet = format!("10.88.{octet}.0/24");
        let bridge_gateway = format!("10.88.{octet}.1");

        // Merge previously persisted project/global approvals into the policy.
        let mut policy = security.network_policy.clone();
        super::approvals::merge_into(&mut policy, &super::approvals::load(root));

        let policy_file = std::env::temp_dir().join(format!("cowboy-policy-{hash:08x}.json"));
        let json = serde_json::to_string_pretty(&policy).context("serializing network policy")?;
        std::fs::write(&policy_file, json)
            .with_context(|| format!("writing policy file {}", policy_file.display()))?;
        // Owner-only: this shared-/tmp file isn't secret, but it shouldn't expose
        // the project's network posture (allow/deny lists) to other local users.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&policy_file, std::fs::Permissions::from_mode(0o600));
        }

        // Control channel: the gateway (sidecar, in the agent's netns) dials the
        // host over TCP for `ask` decisions. A deterministic per-project port; the
        // gateway client retries to absorb the startup race. A fresh random token
        // gates the port (the agent shares the netns but never sees the token).
        let control_port = 9000 + (hash % 1000) as u16;
        // On Linux the host owns the bridge gateway IP: the gateway dials it and
        // the host binds the same address. On Docker Desktop (Mac/Windows) the
        // host has no bridge interface, so the host binds loopback and the gateway
        // reaches it via `host.docker.internal` (in the agent's inherited
        // /etc/hosts). Loopback isn't LAN-exposed, so "never 0.0.0.0" holds.
        let (control_addr, control_bind_addr) = if cfg!(target_os = "linux") {
            let addr = format!("{bridge_gateway}:{control_port}");
            (addr.clone(), addr)
        } else {
            (
                format!("host.docker.internal:{control_port}"),
                format!("127.0.0.1:{control_port}"),
            )
        };
        // Fresh random token per session. An explicit `COWBOY_CONTROL_TOKEN` env
        // pins it instead (used by e2e tests that fake the gateway; also lets ops
        // fix it if needed) — opt-in, so the default stays unguessable.
        let control_token = std::env::var("COWBOY_CONTROL_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());

        let (agent_net, gateway_name) = network_names(hash);
        Ok(Self {
            agent_net,
            gateway_name,
            subnet,
            bridge_gateway,
            policy_file,
            control_addr,
            control_bind_addr,
            control_token,
        })
    }

    /// The agent's egress-capable network name (the sidecar shares its netns).
    pub fn agent_net(&self) -> &str {
        &self.agent_net
    }

    /// `--add-host` entries for the agent container; inherited by the sidecar's
    /// /etc/hosts so it can dial the host control server via `host.docker.internal`
    /// on Docker Desktop. Harmless on Linux (host-gateway maps to the bridge host).
    pub fn agent_extra_hosts(&self) -> Vec<String> {
        vec!["host.docker.internal:host-gateway".into()]
    }

    /// The address the gateway dials (its `COWBOY_CONTROL_ADDR`).
    pub fn control_addr(&self) -> &str {
        &self.control_addr
    }

    /// The address the host binds its control server on (loopback on Docker
    /// Desktop, the bridge gateway IP on Linux).
    pub fn control_bind_addr(&self) -> &str {
        &self.control_bind_addr
    }

    /// The per-session control token (passed to the gateway via env).
    pub fn control_token(&self) -> &str {
        &self.control_token
    }

    /// Build the gateway sidecar container spec. It joins the agent's netns
    /// (`--network container:<agent>`), which forbids per-container network flags
    /// (`--ip`, `--add-host`, `--dns`); those settings live on the agent and are
    /// inherited here. The gateway runs as root (uid 0) so the nft rule can exempt
    /// its own egress (and Docker's embedded resolver) by `skuid 0`; the agent is
    /// kept non-root so it never inherits that exemption.
    fn gateway_spec(&self, agent: &str) -> ContainerSpec {
        let policy = self.policy_file.to_string_lossy().into_owned();
        ContainerSpec {
            name: self.gateway_name.clone(),
            image: GATEWAY_IMAGE.to_string(),
            mounts: vec![BindMount::ro(policy, "/etc/cowboy/policy.json")],
            env: vec![
                (
                    "COWBOY_POLICY_FILE".into(),
                    "/etc/cowboy/policy.json".into(),
                ),
                ("COWBOY_AGENT_SUBNET".into(), self.subnet.clone()),
                ("COWBOY_CONTROL_ADDR".into(), self.control_addr.clone()),
                ("COWBOY_CONTROL_TOKEN".into(), self.control_token.clone()),
            ],
            network: Some(format!("container:{agent}")),
            cap_add: vec!["NET_ADMIN".into(), "NET_RAW".into()],
            // Run the image ENTRYPOINT (cowboy-gateway) with no extra args.
            keep_alive: Some(vec![]),
            ..Default::default()
        }
    }

    /// Create the agent's egress network and verify the gateway image is present.
    /// Called **before** the agent starts so it never runs un-sandboxed.
    pub async fn ensure_network(&self, docker: &dyn DockerCli) -> Result<()> {
        if !docker.network_exists(&self.agent_net).await? {
            docker
                .create_network(&NetworkSpec {
                    // Egress-capable: the agent has a route out, but the sidecar's
                    // nft REDIRECT in the shared netns forces it through the proxy.
                    name: self.agent_net.clone(),
                    internal: false,
                    subnet: Some(self.subnet.clone()),
                    gateway: Some(self.bridge_gateway.clone()),
                })
                .await?;
        }
        super::runtime::ensure_image_available(
            docker,
            GATEWAY_IMAGE,
            "gateway.Dockerfile",
            super::runtime::default_image_source_root().as_deref(),
        )
        .await
        .context("ensuring the gateway image (refusing to run the agent unsandboxed)")
    }

    /// Start (or restart) the gateway sidecar in the agent's netns. Must be called
    /// after the agent container exists. A sidecar from a prior agent lifetime has
    /// exited (its netns is gone), so a non-running one is removed and recreated.
    pub async fn start_sidecar(&self, docker: &dyn DockerCli, agent: &str) -> Result<()> {
        match docker.container_state(&self.gateway_name).await? {
            super::docker::ContainerState::Running => return Ok(()),
            super::docker::ContainerState::Stopped => {
                docker.remove(&self.gateway_name, true).await?;
                docker.run_detached(&self.gateway_spec(agent)).await?;
            }
            super::docker::ContainerState::Absent => {
                docker.run_detached(&self.gateway_spec(agent)).await?;
            }
        }
        // `run_detached` returns once the container is started, not once the
        // in-process proxy has bound its listeners. The agent's nft REDIRECT is
        // already active by then, so a command run in this window has its egress
        // sent to a not-yet-listening :8443 and fails with "connection refused".
        // Wait for the proxy before returning so the first command never races it.
        self.wait_ready(docker).await
    }

    /// Block until the gateway's transparent proxy listener is bound in the
    /// shared netns. Fails closed: if the proxy never comes up we refuse rather
    /// than let the agent run against a half-initialized gateway.
    async fn wait_ready(&self, docker: &dyn DockerCli) -> Result<()> {
        // The transparent listener port (mirrors cowboy-gateway's `PORT_TLS`);
        // its presence means the proxy is accepting REDIRECTed egress.
        const PROXY_LISTEN_MARKER: &str = ":8443";
        let probe = ["ss".to_string(), "-ltn".to_string()];
        for _ in 0..100 {
            if let Ok((res, out)) = docker
                .exec_capture(&self.gateway_name, "/", "root", &probe)
                .await
            {
                if res.exit_code == 0 && out.contains(PROXY_LISTEN_MARKER) {
                    return Ok(());
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        anyhow::bail!(
            "gateway proxy never bound its listener; refusing to run the agent unsandboxed"
        )
    }

    /// Capabilities the agent container must **drop**. `NET_ADMIN`/`NET_RAW` stop
    /// it altering the shared-netns nft rules or sending raw packets; `SETUID`/
    /// `SETGID` stop a (root) agent from changing identity — defence in depth atop
    /// the non-root remap that keeps it from matching the gateway's `skuid 0`
    /// exemption in the first place.
    pub fn agent_caps(&self) -> Vec<String> {
        vec![
            "NET_ADMIN".into(),
            "NET_RAW".into(),
            "SETUID".into(),
            "SETGID".into(),
        ]
    }
}
