//! Host-side bring-up of the sole-egress network gateway.
//!
//! Topology (per project, to avoid cross-project clashes):
//! ```text
//! agent  ──(cowboy-int, --internal)──► gateway(.2) ──(cowboy-egr)──► internet
//!         default route forced to .2          applies allow/deny/ask
//! ```
//! The agent is attached to an internal-only network with its `NET_ADMIN`/
//! `NET_RAW` capabilities dropped, and its default route is forced to the
//! gateway by a short-lived privileged helper that shares the agent's netns.
//! Because the agent never holds `NET_ADMIN`, it cannot undo the route — the
//! only path out is the gateway, which fails closed.

use std::path::PathBuf;

use anyhow::{Context, Result};
use cowboy_core::config::SecurityConfig;

use super::docker::{BindMount, ContainerSpec, DockerCli, NetworkSpec};

/// The gateway image (built from `docker/gateway.Dockerfile`).
pub const GATEWAY_IMAGE: &str = "cowboy/gateway:local";

/// Per-project docker object names derived from the project hash:
/// `(internal_net, egress_net, gateway_container)`.
pub fn network_names(hash: u32) -> (String, String, String) {
    (
        format!("cowboy-int-{hash:08x}"),
        format!("cowboy-egr-{hash:08x}"),
        format!("cowboy-gw-{hash:08x}"),
    )
}

/// Per-project gateway networking parameters.
#[derive(Debug, Clone)]
pub struct GatewayNetwork {
    pub internal_net: String,
    pub egress_net: String,
    pub gateway_name: String,
    pub subnet: String,
    pub gateway_ip: String,
    pub bridge_gateway: String,
    pub egress_subnet: String,
    policy_file: PathBuf,
    /// Host control socket path the gateway connects to for `ask` decisions.
    control_sock: PathBuf,
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
        let gateway_ip = format!("10.88.{octet}.2");
        let egress_subnet = format!("10.89.{octet}.0/24");

        // Merge previously persisted project/global approvals into the policy.
        let mut policy = security.network_policy.clone();
        super::approvals::merge_into(&mut policy, &super::approvals::load(root));

        let policy_file = std::env::temp_dir().join(format!("cowboy-policy-{hash:08x}.json"));
        let json = serde_json::to_string_pretty(&policy).context("serializing network policy")?;
        std::fs::write(&policy_file, json)
            .with_context(|| format!("writing policy file {}", policy_file.display()))?;

        // Host-writable runtime dir for the control socket (mounted into the
        // gateway). Avoids needing root-owned /run.
        let run_dir = std::env::temp_dir().join("cowboy-run");
        let _ = std::fs::create_dir_all(&run_dir);
        let control_sock = run_dir.join(format!("control-{hash:08x}.sock"));

        let (internal_net, egress_net, gateway_name) = network_names(hash);
        Ok(Self {
            internal_net,
            egress_net,
            gateway_name,
            subnet,
            gateway_ip,
            bridge_gateway,
            egress_subnet,
            policy_file,
            control_sock,
        })
    }

    /// The container path the control socket is mounted at.
    fn control_sock_container_path(&self) -> String {
        let name = self
            .control_sock
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("control.sock");
        format!("/run/cowboy/{name}")
    }

    /// Build the gateway container spec.
    fn gateway_spec(&self) -> ContainerSpec {
        let policy = self.policy_file.to_string_lossy().into_owned();
        let sock_dir = self
            .control_sock
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/tmp".into());
        ContainerSpec {
            name: self.gateway_name.clone(),
            image: GATEWAY_IMAGE.to_string(),
            workdir: String::new(),
            mounts: vec![
                BindMount::ro(policy, "/etc/cowboy/policy.json"),
                // Mount the runtime dir so the host-created socket is visible.
                BindMount::rw(sock_dir, "/run/cowboy"),
            ],
            env: vec![
                (
                    "COWBOY_POLICY_FILE".into(),
                    "/etc/cowboy/policy.json".into(),
                ),
                ("COWBOY_AGENT_SUBNET".into(), self.subnet.clone()),
                (
                    "COWBOY_CONTROL_SOCK".into(),
                    self.control_sock_container_path(),
                ),
            ],
            network: Some(self.internal_net.clone()),
            ip: Some(self.gateway_ip.clone()),
            cap_add: vec!["NET_ADMIN".into(), "NET_RAW".into()],
            sysctls: vec![
                ("net.ipv4.ip_forward".into(), "1".into()),
                ("net.ipv4.conf.all.route_localnet".into(), "1".into()),
            ],
            // Run the image ENTRYPOINT (cowboy-gateway) with no extra args.
            keep_alive: Some(vec![]),
            ..Default::default()
        }
    }

    /// Ensure the networks and the gateway container are up.
    pub async fn ensure(&self, docker: &dyn DockerCli) -> Result<()> {
        if !docker.network_exists(&self.internal_net).await? {
            docker
                .create_network(&NetworkSpec {
                    name: self.internal_net.clone(),
                    internal: true,
                    subnet: Some(self.subnet.clone()),
                    gateway: Some(self.bridge_gateway.clone()),
                })
                .await?;
        }
        if !docker.network_exists(&self.egress_net).await? {
            docker
                .create_network(&NetworkSpec {
                    name: self.egress_net.clone(),
                    internal: false,
                    subnet: Some(self.egress_subnet.clone()),
                    gateway: None,
                })
                .await?;
        }

        if !docker.image_exists(GATEWAY_IMAGE).await? {
            anyhow::bail!(
                "gateway image {GATEWAY_IMAGE} not found; build it with:\n  \
                 docker build -f docker/gateway.Dockerfile -t {GATEWAY_IMAGE} ."
            );
        }

        match docker.container_state(&self.gateway_name).await? {
            super::docker::ContainerState::Running => {}
            super::docker::ContainerState::Stopped => {
                docker.remove(&self.gateway_name, true).await?;
                self.start_gateway(docker).await?;
            }
            super::docker::ContainerState::Absent => self.start_gateway(docker).await?,
        }
        Ok(())
    }

    async fn start_gateway(&self, docker: &dyn DockerCli) -> Result<()> {
        docker.run_detached(&self.gateway_spec()).await?;
        // Add the egress NIC so the gateway can reach the internet.
        docker
            .connect_network(&self.egress_net, &self.gateway_name)
            .await
            .context("attaching egress network to gateway")?;
        Ok(())
    }

    /// Capabilities and DNS settings the agent container must use.
    pub fn agent_caps(&self) -> Vec<String> {
        vec!["NET_ADMIN".into(), "NET_RAW".into()]
    }

    /// Force the agent's default route through the gateway, from a short-lived
    /// privileged helper sharing the agent's network namespace. The agent
    /// itself never holds `NET_ADMIN`, so it cannot reverse this.
    pub async fn force_agent_route(&self, docker: &dyn DockerCli, agent: &str) -> Result<()> {
        let script = format!(
            "ip route replace default via {gw} && \
             (ip route add blackhole 169.254.169.254/32 2>/dev/null || true)",
            gw = self.gateway_ip
        );
        let helper = ContainerSpec {
            image: GATEWAY_IMAGE.to_string(),
            network: Some(format!("container:{agent}")),
            cap_add: vec!["NET_ADMIN".into()],
            // Override the image ENTRYPOINT (cowboy-gateway) to run the route
            // commands instead.
            entrypoint: Some("sh".into()),
            keep_alive: Some(vec!["-c".into(), script]),
            ..Default::default()
        };
        let res = docker
            .run_oneshot(&helper)
            .await
            .context("forcing agent default route via gateway")?;
        if res.exit_code != 0 {
            anyhow::bail!("route helper exited with {}", res.exit_code);
        }
        Ok(())
    }

    /// Path to the host control socket.
    pub fn control_sock(&self) -> &std::path::Path {
        &self.control_sock
    }
}
