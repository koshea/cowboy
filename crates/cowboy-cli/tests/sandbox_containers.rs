//! The container proxy against a real engine: Compose and a published port work,
//! and the escapes are refused.
//!
//! Needs the sandbox and a reachable container engine (rootless podman or Docker)
//! with `nginx:alpine` and `alpine` already pulled, and is **opt-in**: it starts
//! real containers on your engine, so it runs only with `COWBOY_DOCKER_TESTS` set.
//! `COWBOY_DOCKER_TESTS=required` also turns a skip into a failure, so a broken
//! probe cannot make this file pass while doing nothing.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cowboy_cli::sandbox::native::NativeSandbox;
use cowboy_cli::sandbox::Sandbox;
use cowboy_core::config::SecurityConfig;
use cowboy_sandbox::HostProbe;

struct Host;

impl HostProbe for Host {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn git_common_dir(&self, _root: &Path) -> Option<PathBuf> {
        None
    }
    fn expand(&self, raw: &str) -> Option<PathBuf> {
        cowboy_core::config::expand_path(raw).ok()
    }
    fn home(&self) -> Option<PathBuf> {
        cowboy_core::config::expand_path("~").ok()
    }
    fn self_exe(&self) -> Option<PathBuf> {
        let dir = std::env::current_exe()
            .ok()?
            .parent()?
            .parent()?
            .to_path_buf();
        let exe = dir.join("cowboy");
        exe.exists().then_some(exe)
    }
    fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
        std::fs::canonicalize(path).ok()
    }
}

const COMPOSE: &str = "/usr/libexec/docker/cli-plugins/docker-compose";

/// The real engine socket, for the test's own before/after checks.
fn engine() -> Option<PathBuf> {
    cowboy_cli::sandbox::containers::engine_socket(None)
}

fn unsupported() -> Option<String> {
    if std::env::var_os("COWBOY_DOCKER_TESTS").is_none() {
        return Some("opt-in: set COWBOY_DOCKER_TESTS=1 (or =required)".into());
    }
    if cowboy_cli::sandbox::bwrap::resolve_bwrap().is_err() {
        return Some("bubblewrap not available".into());
    }
    if Host.self_exe().is_none() {
        return Some("the cowboy binary is not built alongside the test".into());
    }
    if !Path::new(COMPOSE).exists() {
        return Some(format!("{COMPOSE} not installed"));
    }
    let Some(sock) = engine() else {
        return Some("no container engine socket".into());
    };
    for image in ["nginx:alpine", "alpine:latest"] {
        let ok = std::process::Command::new("curl")
            .args(["-sf", "--unix-socket"])
            .arg(&sock)
            .arg(format!("http://d/images/{image}/json"))
            .stdout(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return Some(format!("image {image} is not present (pull it first)"));
        }
    }
    None
}

macro_rules! skip_if_unsupported {
    () => {
        if let Some(why) = unsupported() {
            if std::env::var("COWBOY_DOCKER_TESTS").as_deref() == Ok("required") {
                panic!("container tests required but unsupported here: {why}");
            }
            eprintln!("skipping: {why}");
            return;
        }
    };
}

fn sandbox(root: &Path) -> NativeSandbox {
    let mut security = SecurityConfig::default();
    security.sandbox.containers.enabled = true;
    NativeSandbox::new(
        root.to_path_buf(),
        security,
        Box::new(Host),
        Arc::new(cowboy_gateway::DenyAll),
    )
    .unwrap()
}

async fn run(s: &NativeSandbox, command: &str) -> (i32, String) {
    let (res, out) = s.run_capture(command, None, 300).await.unwrap();
    (res.exit_code, out)
}

/// Ask the engine (not the proxy) for containers with a label.
fn labelled(label: &str) -> usize {
    let filters = format!("{{\"label\":[\"{label}\"]}}");
    let out = std::process::Command::new("curl")
        .args(["-sf", "--unix-socket"])
        .arg(engine().unwrap())
        .arg("-G")
        .arg("http://d/containers/json")
        .args(["--data-urlencode", "all=1", "--data-urlencode"])
        .arg(format!("filters={filters}"))
        .output()
        .unwrap();
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|v| v.as_array().map(Vec::len))
        .unwrap_or(0)
}

/// One raw Docker API call through the proxy, from inside the sandbox.
fn api(method: &str, path: &str, body: &str) -> String {
    format!(
        r#"python3 - <<'PY'
import socket, json
s = socket.socket(socket.AF_UNIX); s.connect("/run/cowboy/docker.sock")
body = {body:?}.encode()
s.sendall(("{method} {path} HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\nContent-Length: %d\r\n\r\n" % len(body)).encode() + body)
out = b""
while True:
    c = s.recv(65536)
    if not c: break
    out += c
print(out.split(b"\r\n", 1)[0].decode(), out.split(b"\r\n\r\n", 1)[1][:300].decode(errors="replace"))
PY"#
    )
}

#[tokio::test]
async fn compose_and_published_ports_work_and_escapes_are_refused() {
    skip_if_unsupported!();
    let dir = assert_fs::TempDir::new().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(root.join(".cowboy")).unwrap();
    std::fs::write(root.join(".cowboy/security.yaml"), "version: 1\n").unwrap();
    std::fs::create_dir_all(root.join("site")).unwrap();
    std::fs::write(root.join("site/index.html"), "hello from the project\n").unwrap();
    std::fs::write(
        root.join("compose.yaml"),
        "services:\n  web:\n    image: nginx:alpine\n    ports: [\"80\"]\n    volumes:\n      \
         - ./site:/usr/share/nginx/html:ro\n  probe:\n    image: alpine:latest\n    command: \
         [\"sh\", \"-c\", \"wget -qO- http://web/ && sleep 60\"]\n    depends_on: [web]\n",
    )
    .unwrap();

    let s = sandbox(&root);
    let compose = format!("COMPOSE_PROJECT_NAME=cbtest {COMPOSE}");

    let (code, out) = run(&s, &format!("{compose} up -d 2>&1")).await;
    assert_eq!(code, 0, "compose up: {out}");

    // A published port is reachable on the sandbox's localhost.
    let (_, port) = run(&s, &format!("{compose} port web 80")).await;
    let port: u16 = port
        .trim()
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("no published port: {port}"));
    let mut fetched = String::new();
    for _ in 0..20 {
        let (_, o) = run(
            &s,
            &format!(
                "python3 -c 'import urllib.request;print(urllib.request.urlopen(\"http://127.0.0.1:{port}/\",timeout=3).read().decode())' 2>&1"
            ),
        )
        .await;
        fetched = o;
        if fetched.contains("hello from the project") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(fetched.contains("hello from the project"), "{fetched}");

    // Services reach each other by name, and nothing beyond.
    let (_, logs) = run(&s, &format!("{compose} logs probe 2>&1")).await;
    assert!(logs.contains("hello from the project"), "{logs}");
    let (_, out) = run(
        &s,
        &format!("{compose} exec -T probe sh -c 'wget -qO- -T 4 http://1.1.1.1/ && echo LEAK || echo NOEGRESS' 2>&1"),
    )
    .await;
    assert!(
        out.contains("NOEGRESS"),
        "containers must have no route out: {out}"
    );

    // The escapes, each refused by the proxy.
    for (what, method, path, body) in [
        (
            "a host bind",
            "POST",
            "/containers/create",
            r#"{"Image":"alpine:latest","HostConfig":{"Binds":["/:/host"]}}"#,
        ),
        (
            "privileged",
            "POST",
            "/containers/create",
            r#"{"Image":"alpine:latest","HostConfig":{"Privileged":true}}"#,
        ),
        (
            "host network",
            "POST",
            "/containers/create",
            r#"{"Image":"alpine:latest","HostConfig":{"NetworkMode":"host"}}"#,
        ),
        (
            "the engine socket",
            "POST",
            "/containers/create",
            r#"{"Image":"alpine:latest","HostConfig":{"Binds":["/run/user/1000/podman/podman.sock:/s"]}}"#,
        ),
        ("a push", "POST", "/images/alpine/push", ""),
        (
            "a disallowed registry",
            "POST",
            "/images/create?fromImage=evil.example/x&tag=1",
            "",
        ),
        ("libpod", "GET", "/v4.0.0/libpod/info", ""),
    ] {
        let (_, out) = run(&s, &api(method, path, body)).await;
        assert!(
            out.contains("403") && out.contains("cowboy"),
            "{what}: {out}"
        );
    }

    // The listing shows only this session's containers.
    let (_, list) = run(&s, &api("GET", "/containers/json?all=1", "")).await;
    let ours = labelled("com.docker.compose.project=cbtest");
    assert!(ours >= 2, "the stack's containers exist on the engine");
    assert!(list.starts_with("HTTP/1.1 200"), "{list}");

    s.stop().await;
    // Positive first (above), then: nothing of the session survives.
    assert_eq!(
        labelled("com.docker.compose.project=cbtest"),
        0,
        "the session sweep"
    );
}
