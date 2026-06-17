//! End-to-end proof of the network security thesis (Slice C).
//!
//! Brings up the real topology — internal-only agent, sole-egress gateway,
//! forced default route, dropped caps — and asserts:
//!   * an allow-listed destination is reachable through the gateway,
//!   * an un-listed destination is blocked (fail-closed; no host approver),
//!   * the cloud metadata endpoint is denied,
//!   * a non-80/443 port is dropped (the gateway is not an open router).
//!
//! Marked `#[ignore]`: it builds the gateway image and runs several containers,
//! so it is opt-in (`cargo test -- --ignored gateway`). Skips if Docker is
//! absent.

use std::collections::HashSet;
use std::process::Command as Std;

use assert_cmd::Command;
use assert_fs::prelude::*;

const GATEWAY_IMAGE: &str = "cowboy/gateway:local";

fn docker_available() -> bool {
    Std::new("docker")
        .args(["info"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn repo_root() -> std::path::PathBuf {
    // crates/cowboy-cli -> repo root
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Build the gateway image if it is not already present.
fn ensure_gateway_image() {
    let present = Std::new("docker")
        .args(["image", "inspect", GATEWAY_IMAGE])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if present {
        return;
    }
    eprintln!("building {GATEWAY_IMAGE} (one-time, may take a few minutes)...");
    let status = Std::new("docker")
        .current_dir(repo_root())
        .args([
            "build",
            "-f",
            "docker/gateway.Dockerfile",
            "-t",
            GATEWAY_IMAGE,
            ".",
        ])
        .status()
        .unwrap();
    assert!(status.success(), "gateway image build failed");
}

/// IDs of cowboy-labelled containers and networks (for snapshot-diff cleanup).
fn cowboy_objects() -> (HashSet<String>, HashSet<String>) {
    let ids = |args: &[&str]| -> HashSet<String> {
        Std::new("docker")
            .args(args)
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    (
        ids(&["ps", "-aq", "--filter", "label=cowboy=1"]),
        ids(&["network", "ls", "-q", "--filter", "label=cowboy=1"]),
    )
}

#[test]
#[ignore = "builds gateway image and runs multiple containers"]
fn network_boundary_is_enforced() {
    if !docker_available() {
        eprintln!("skipping: docker not available");
        return;
    }
    ensure_gateway_image();

    let (containers_before, networks_before) = cowboy_objects();

    let tmp = assert_fs::TempDir::new().unwrap();
    // Allow only 1.1.1.1:80 through the gateway; everything else asks (and with
    // no host approver, fails closed). Metadata is denied by default.
    tmp.child(".cowboy/security.yaml")
        .write_str(
            "version: 1\n\
             container:\n\
             \x20 image: busybox:latest\n\
             \x20 workdir: /workspace\n\
             \x20 mounts:\n\
             \x20   - source: .\n\
             \x20     target: /workspace\n\
             \x20     mode: rw\n\
             network_policy:\n\
             \x20 default_external: ask\n\
             \x20 allow:\n\
             \x20   cidrs: [\"1.1.1.1/32\"]\n\
             \x20   ports: [443]\n\
             \x20 deny:\n\
             \x20   cidrs: [\"169.254.169.254/32\"]\n",
        )
        .unwrap();
    tmp.child(".cowboy/agent.yaml")
        .write_str("version: 1\n")
        .unwrap();
    tmp.child(".cowboy/models.yaml")
        .write_str("version: 1\n")
        .unwrap();

    let agent_name = format!("cowboy-e2e-agent-{}", std::process::id());

    let cowboy = |args: &[&str]| -> std::process::Output {
        Command::cargo_bin("cowboy")
            .unwrap()
            .current_dir(tmp.path())
            .env("COWBOY_CONTAINER_NAME", &agent_name)
            .args(args)
            .output()
            .unwrap()
    };

    // wget helper: succeeds (exit 0) only if the request completes. We use
    // direct https (no redirects) so the destination IP/port is exactly what we
    // allow-list, avoiding http->https->hostname redirect confounds.
    let wget = |url: &str| -> bool {
        cowboy(&["run", "wget", "-q", "-T", "10", "-O", "/dev/null", url])
            .status
            .success()
    };

    // 1. Allowed destination reachable through the transparent TLS proxy.
    let allowed = wget("https://1.1.1.1");
    // 2. Un-listed destination blocked (ask -> fail-closed deny).
    let unlisted = wget("https://1.0.0.1");
    // 3. Metadata endpoint denied (blackholed at the agent + denied by policy).
    let metadata = wget("http://169.254.169.254");
    // 4. Non-80/443 port dropped by the forward chain (not an open router).
    let high_port = wget("https://1.1.1.1:9999");
    // 5. The agent cannot change its own network config (NET_ADMIN dropped),
    //    so it cannot escape the forced route.
    let route_change_denied = !cowboy(&["run", "ip", "link", "set", "eth0", "down"])
        .status
        .success();

    // Cleanup BEFORE asserting so a failure never leaks containers/networks.
    let _ = Std::new("docker").args(["rm", "-f", &agent_name]).output();
    let (containers_after, networks_after) = cowboy_objects();
    for id in containers_after.difference(&containers_before) {
        let _ = Std::new("docker").args(["rm", "-f", id]).output();
    }
    for id in networks_after.difference(&networks_before) {
        let _ = Std::new("docker").args(["network", "rm", id]).output();
    }

    assert!(
        allowed,
        "allow-listed 1.1.1.1:443 should be reachable via the gateway"
    );
    assert!(!unlisted, "un-listed 1.0.0.1 must be blocked (fail-closed)");
    assert!(!metadata, "metadata endpoint must be denied");
    assert!(
        !high_port,
        "non-80/443 port must be dropped (gateway is not an open router)"
    );
    assert!(
        route_change_denied,
        "agent must not be able to change its network config (NET_ADMIN dropped)"
    );
}

/// DNS resolution is policy-gated (strict): a name the policy allows resolves and
/// connects through the gateway; an un-allowed name is REFUSED at the resolver and
/// can't even be looked up — closing DNS as an exfiltration channel. Needs Docker
/// + internet egress (resolves the allowed name upstream).
#[test]
#[ignore = "builds gateway image and runs multiple containers + needs internet"]
fn dns_resolution_is_policy_gated() {
    if !docker_available() {
        eprintln!("skipping: docker not available");
        return;
    }
    ensure_gateway_image();

    let (containers_before, networks_before) = cowboy_objects();
    let tmp = assert_fs::TempDir::new().unwrap();
    // Allow the cloudflare.com domain (name + 443); everything else asks → with no
    // approver, fails closed. DNS policy defaults (strict enforce + tunnel
    // detection + risky-qtype refusal) apply.
    tmp.child(".cowboy/security.yaml")
        .write_str(
            "version: 1\n\
             container:\n\
             \x20 image: busybox:latest\n\
             \x20 workdir: /workspace\n\
             \x20 mounts:\n\
             \x20   - source: .\n\
             \x20     target: /workspace\n\
             \x20     mode: rw\n\
             network_policy:\n\
             \x20 default_external: ask\n\
             \x20 allow:\n\
             \x20   domains: [\"cloudflare.com\"]\n\
             \x20   ports: [443]\n",
        )
        .unwrap();
    tmp.child(".cowboy/agent.yaml")
        .write_str("version: 1\n")
        .unwrap();
    tmp.child(".cowboy/models.yaml")
        .write_str("version: 1\n")
        .unwrap();

    let agent_name = format!("cowboy-e2e-dns-{}", std::process::id());
    let cowboy = |args: &[&str]| -> std::process::Output {
        Command::cargo_bin("cowboy")
            .unwrap()
            .current_dir(tmp.path())
            .env("COWBOY_CONTAINER_NAME", &agent_name)
            .args(args)
            .output()
            .unwrap()
    };
    // wget completes only if the name RESOLVES (DNS allowed) and the connection is
    // allowed. An un-allowed name fails at resolution.
    let wget = |url: &str| -> bool {
        cowboy(&["run", "wget", "-q", "-T", "10", "-O", "/dev/null", url])
            .status
            .success()
    };

    let allowed = wget("https://cloudflare.com"); // name allowed → resolves + connects
    let refused = wget("https://example.com"); // not allowed → DNS REFUSED

    let _ = Std::new("docker").args(["rm", "-f", &agent_name]).output();
    let (containers_after, networks_after) = cowboy_objects();
    for id in containers_after.difference(&containers_before) {
        let _ = Std::new("docker").args(["rm", "-f", id]).output();
    }
    for id in networks_after.difference(&networks_before) {
        let _ = Std::new("docker").args(["network", "rm", id]).output();
    }

    assert!(allowed, "allow-listed cloudflare.com should resolve + connect");
    assert!(
        !refused,
        "un-allowed example.com must be REFUSED at the resolver (no exfil channel)"
    );
}
