//! End-to-end proof that the **approval path** works: when the policy yields
//! `ask`, the gateway reaches the host control socket, the host approves, and
//! the connection is then allowed through.
//!
//! This is the interactive counterpart to `gateway_e2e` (which proves
//! fail-closed denial when there is *no* approver). Here we bind a host approver
//! at the exact control-socket path the gateway uses and approve everything,
//! then assert an otherwise-blocked destination becomes reachable and that the
//! approver actually received an `ask`.
//!
//! `#[ignore]`: builds the gateway image and runs containers. Run with
//! `cargo test -- --ignored approval`.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command as Std;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use assert_fs::prelude::*;
use cowboy_core::netproto::{encode_line, ApprovalScope, GatewayMessage, HostMessage, Verdict};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

fn docker_available() -> bool {
    Std::new("docker")
        .args(["info"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Mirror of `runtime::project_hash` + `GatewayNetwork::for_project` socket path.
fn control_sock_for(root: &Path) -> PathBuf {
    let mut h = DefaultHasher::new();
    root.hash(&mut h);
    let hash = h.finish() as u32;
    std::env::temp_dir()
        .join("cowboy-run")
        .join(format!("control-{hash:08x}.sock"))
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "builds gateway image and runs containers"]
async fn host_approval_unblocks_an_otherwise_denied_destination() {
    if !docker_available() {
        eprintln!("skipping: docker not available");
        return;
    }

    let tmp = assert_fs::TempDir::new().unwrap();
    // Canonicalize so the path matches what the child's `current_dir()` yields,
    // keeping our hash in sync with the gateway's.
    let root = tmp.path().canonicalize().unwrap();
    // Allow nothing by default -> external = ask. Metadata denied.
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
             \x20 default_external: ask\n",
        )
        .unwrap();
    tmp.child(".cowboy/agent.yaml")
        .write_str("version: 1\n")
        .unwrap();

    // Bind the host approver at the exact path the gateway will dial.
    let sock = control_sock_for(&root);
    let _ = std::fs::create_dir_all(sock.parent().unwrap());
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).expect("bind control socket");
    let asks = Arc::new(AtomicUsize::new(0));
    let asks2 = asks.clone();
    let approver = tokio::spawn(async move {
        // Approve every ask for this session.
        while let Ok((stream, _)) = listener.accept().await {
            let asks = asks2.clone();
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut reader = BufReader::new(r);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    if let Ok(GatewayMessage::Ask { id, .. }) =
                        serde_json::from_str::<GatewayMessage>(line.trim())
                    {
                        asks.fetch_add(1, Ordering::SeqCst);
                        let d = HostMessage::Decision {
                            id,
                            verdict: Verdict::Allow,
                            scope: ApprovalScope::Session,
                        };
                        if w.write_all(encode_line(&d).as_bytes()).await.is_err() {
                            break;
                        }
                        let _ = w.flush().await;
                    }
                }
            });
        }
    });

    let agent_name = format!("cowboy-approval-e2e-{}", std::process::id());
    let (containers_before, networks_before) = cowboy_objects();

    let root2 = root.clone();
    let agent2 = agent_name.clone();
    // `cowboy run wget …` blocks; run it off the async executor.
    let success = tokio::task::spawn_blocking(move || {
        Std::new(env!("CARGO_BIN_EXE_cowboy"))
            .current_dir(&root2)
            .env("COWBOY_CONTAINER_NAME", &agent2)
            // 1.0.0.1 is not allow-listed -> ask -> our approver allows it.
            .args([
                "run",
                "wget",
                "-q",
                "-T",
                "20",
                "-O",
                "/dev/null",
                "https://1.0.0.1",
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
    .await
    .unwrap();

    // Cleanup before asserting so failures never leak containers/networks.
    approver.abort();
    let _ = std::fs::remove_file(&sock);
    let _ = Std::new("docker").args(["rm", "-f", &agent_name]).output();
    let (containers_after, networks_after) = cowboy_objects();
    for id in containers_after.difference(&containers_before) {
        let _ = Std::new("docker").args(["rm", "-f", id]).output();
    }
    for id in networks_after.difference(&networks_before) {
        let _ = Std::new("docker").args(["network", "rm", id]).output();
    }

    let n_asks = asks.load(Ordering::SeqCst);
    assert!(
        n_asks > 0,
        "the gateway must reach the host approver with an ask"
    );
    assert!(
        success,
        "an approved destination must become reachable through the gateway"
    );
}
