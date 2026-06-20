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

// Use the runtime's own image name so the locally-built tag matches what the
// gateway expects (version-pinned GHCR name).
use cowboy_cli::net::gateway::GATEWAY_IMAGE;

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
    // Allow Cloudflare's DNS anycast pair (1.1.1.1 + 1.0.0.1) on :443; everything
    // else asks (and with no host approver, fails closed). Metadata is denied by
    // default. Both IPs are listed deliberately: `https://1.1.1.1` 301-redirects to
    // `https://one.one.one.one/`, which busybox wget follows, and that name
    // resolves to *both* anycast addresses — so allow-listing only one would make
    // reachability a coin-flip on which address the redirect lands on. We prove a
    // non-listed-but-reachable IP is blocked separately (8.8.8.8 below).
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
             \x20   cidrs: [\"1.1.1.1/32\", \"1.0.0.1/32\"]\n\
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

    // wget helper: succeeds (exit 0) only if the request completes (including any
    // redirect busybox follows; the allow-list covers the redirect target above).
    let wget = |url: &str| -> bool {
        cowboy(&["run", "wget", "-q", "-T", "10", "-O", "/dev/null", url])
            .status
            .success()
    };

    // 1. Allowed destination reachable through the transparent TLS proxy.
    let allowed = wget("https://1.1.1.1");
    // 2. A reachable but un-listed destination is blocked (ask -> fail-closed
    //    deny). 8.8.8.8 is up, so this proves the gateway blocks by policy rather
    //    than the address merely being unreachable.
    let unlisted = wget("https://8.8.8.8");
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
    assert!(
        !unlisted,
        "un-listed but reachable 8.8.8.8 must be blocked (fail-closed)"
    );
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

    assert!(
        allowed,
        "allow-listed cloudflare.com should resolve + connect"
    );
    assert!(
        !refused,
        "un-allowed example.com must be REFUSED at the resolver (no exfil channel)"
    );
}

/// A malicious agent cannot reach an arbitrary IP by presenting an allow-listed
/// SNI. Egress is authorized by the hostname the *gateway* resolved for the dialed
/// IP, not by the client-controlled SNI. We allow `cloudflare.com`, then from a
/// non-root peer in the agent's netns (the agent's identity — root is the gateway's
/// own exempt uid): a TLS handshake to cloudflare's real IP with its own SNI
/// succeeds, but a handshake to 8.8.8.8 (reachable, never resolved as cloudflare)
/// while *spoofing* `SNI=cloudflare.com` is denied. Needs Docker + internet.
#[test]
#[ignore = "builds gateway image and runs multiple containers + needs internet"]
fn sni_spoof_cannot_reach_arbitrary_ip() {
    if !docker_available() {
        eprintln!("skipping: docker not available");
        return;
    }
    ensure_gateway_image();

    let (containers_before, networks_before) = cowboy_objects();
    let tmp = assert_fs::TempDir::new().unwrap();
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

    let agent_name = format!("cowboy-e2e-spoof-{}", std::process::id());
    let cowboy = |args: &[&str]| -> std::process::Output {
        Command::cargo_bin("cowboy")
            .unwrap()
            .current_dir(tmp.path())
            .env("COWBOY_CONTAINER_NAME", &agent_name)
            .args(args)
            .output()
            .unwrap()
    };
    // Bring up the agent + gateway sidecar so its netns exists for the probe.
    let _ = cowboy(&["run", "true"]);

    // A non-root openssl probe sharing the agent's netns (so its egress is
    // REDIRECTed through the proxy, exactly like the agent). The gateway image
    // carries openssl + a CA bundle. A `subject=` line means the server's cert was
    // received — i.e. the connection was allowed through; a denied connection is
    // dropped by the proxy and yields `no peer certificate available` instead.
    // (`Verify return code: 0` is printed vacuously even with no cert, so it's not
    // a reliable allow signal.)
    let tls_probe = |connect: &str, sni: &str| -> String {
        let out = Std::new("docker")
            .args([
                "run",
                "--rm",
                "--user",
                "65534:65534",
                "--cap-drop",
                "ALL",
                "--network",
                &format!("container:{agent_name}"),
                "--entrypoint",
                "sh",
                GATEWAY_IMAGE,
                "-c",
                &format!(
                    "echo | timeout 12 openssl s_client -connect {connect} -servername {sni} 2>&1"
                ),
            ])
            .output()
            .unwrap();
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    };

    // Legit: cloudflare.com resolves through the gateway and connects.
    let legit = tls_probe("cloudflare.com:443", "cloudflare.com");
    // Spoof: dial 8.8.8.8 while claiming SNI=cloudflare.com.
    let spoof = tls_probe("8.8.8.8:443", "cloudflare.com");

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
        legit.contains("subject="),
        "allow-listed cloudflare.com should connect (a cert is received); got:\n{legit}"
    );
    assert!(
        !spoof.contains("subject="),
        "spoofing SNI=cloudflare.com to dial 8.8.8.8 must be DENIED (authorize by \
         resolved name, not client SNI — no cert should be received); got:\n{spoof}"
    );
}
