//! Running a crew job on an external agent harness (`harnesses.yaml`) instead of
//! cowboy's own loop.
//!
//! The foreman spawns `cowboy` as a subagent child as usual, with
//! [`ENV_HARNESS`] naming the harness. The child ([`run_child`]) keeps every part of
//! the subagent contract the foreman relies on — journal at the session's
//! `events.jsonl`, final answer on stdout, a non-zero exit with the reason on stderr
//! — and in between runs the vendor CLI **inside its own cowboy sandbox**:
//!
//! - the CLI binary, read-only, and a **private home** holding a copy of the user's
//!   login (and, with `auth: full_home`, their vendor config) — never the real
//!   vendor home, which holds code the host later runs;
//! - the harness's API/login hosts allowed; everything else under the normal
//!   policy, failing closed (nobody can be asked from here);
//! - the CLI's own approvals and sandbox off: cowboy's kernel boundary is the one
//!   that holds, which is the only reason that is safe.
//!
//! No turn or time limit. A job that goes quiet is reported to the foreman as a job
//! update; nothing is ever killed for it.

pub mod grok;
pub mod mcp;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use cowboy_core::harness::{AuthExposure, HarnessDef, HarnessKind, HarnessesConfig, KindSpec};
use cowboy_sandbox::plan::{OverlayBind, PlanOverlay};

use crate::agent::jobctl::{ControlDir, Note};
use crate::agent::ui::AgentUi;
use crate::agent::JournalUi;
use crate::sandbox::Sandbox;

/// Set by the foreman on a subagent child that should run this harness.
pub const ENV_HARNESS: &str = "COWBOY_HARNESS";

/// The harness this process was asked to run, if any.
pub fn requested() -> Option<String> {
    std::env::var(ENV_HARNESS).ok().filter(|s| !s.is_empty())
}

/// The child side: run `task` on harness `name` and report like any subagent.
pub async fn run_child(name: &str, task: Option<String>) -> Result<()> {
    let root = crate::cmd::project_root()?;
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let Some(task) = crate::cmd::session::resolve_task(task)? else {
        bail!("no task given to the {name} harness");
    };
    let harnesses = HarnessesConfig::load_user().context("loading harnesses.yaml")?;
    let Some(def) = harnesses.get(name).cloned() else {
        bail!("no harness `{name}` in ~/.config/cowboy/harnesses.yaml");
    };
    let logger = crate::session::SessionLogger::create(&root).ok();
    let session_id = logger
        .as_ref()
        .map(|l| l.id().to_string())
        .unwrap_or_else(|| format!("{}-{}", cowboy_core::time::now_ms(), std::process::id()));
    let journal = crate::session::session_dir(&root, &session_id).join("events.jsonl");
    let mut ui = JournalUi::new(&journal);

    let outcome = drive(&root, name, &def, &task, &session_id, &mut ui).await;
    match outcome {
        Ok(result) => {
            // Journals it and prints it: stdout is the foreman's copy.
            ui.final_message(&result);
            Ok(())
        }
        Err(e) => {
            ui.notice(&format!("{name}: {e:#}"));
            Err(e)
        }
    }
}

/// What `cowboy harnesses` and `cowboy doctor` report about one harness.
pub struct Inspection {
    /// The resolved binary, or why it could not be found.
    pub binary: std::result::Result<PathBuf, String>,
    /// `--version` output, when the binary runs.
    pub version: Option<String>,
    /// Whether the vendor login file exists on the host.
    pub logged_in: bool,
}

/// Check a harness is usable, without running a job.
pub fn inspect(def: &HarnessDef) -> Inspection {
    let spec = def.kind.spec();
    let binary = resolve_binary(def, &spec).map_err(|e| format!("{e:#}"));
    let version = binary.as_ref().ok().and_then(|b| {
        std::process::Command::new(b)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|v| !v.is_empty())
    });
    let logged_in = cowboy_core::config::expand_path(spec.home)
        .map(|h| h.join(spec.auth_file).is_file())
        .unwrap_or(false);
    Inspection {
        binary,
        version,
        logged_in,
    }
}

/// Everything between "we have a task" and "here is the result".
async fn drive(
    root: &Path,
    name: &str,
    def: &HarnessDef,
    task: &str,
    session_id: &str,
    ui: &mut dyn AgentUi,
) -> Result<String> {
    let spec = def.kind.spec();
    let binary = resolve_binary(def, &spec)
        .with_context(|| format!("the `{}` CLI is not installed", spec.binary))?;
    let vendor_home =
        cowboy_core::config::expand_path(spec.home).map_err(|e| anyhow::anyhow!("{e}"))?;
    let host_auth = vendor_home.join(spec.auth_file);
    if !host_auth.is_file() {
        bail!(
            "not logged in — run `{} login` on the host first (no {})",
            spec.binary,
            host_auth.display()
        );
    }

    // The job's private home: host-only, removed when the job ends.
    let home = private_home(session_id)?;
    let _cleanup = RemoveOnDrop(home.clone());
    let seeded = seed_home(&home, &vendor_home, &spec, def.auth)?;
    let brief = home.join("cowboy-brief.md");
    std::fs::write(&brief, task).context("writing the harness brief")?;

    // Egress: the harness's own hosts, on top of the project's policy.
    let mut security = crate::cmd::sandbox::load(root)?;
    crate::cmd::session::drop_approval_required_grants(&mut security);
    for host in def.hosts() {
        if !security.network_policy.allow.domains.contains(&host) {
            security.network_policy.allow.domains.push(host);
        }
    }
    let session_dir = crate::session::session_dir(root, session_id);
    let logging = crate::cmd::session::autodeny_approver(Some(session_dir));
    // Nobody can be asked from this process; a destination beyond the harness's own
    // hosts is put to the person attached to the foreman instead (fails closed).
    let approver: std::sync::Arc<dyn cowboy_gateway::Approver> = match ControlDir::from_env() {
        Some(control) => std::sync::Arc::new(ForwardingApprover {
            control,
            logging,
            seq: std::sync::atomic::AtomicU32::new(0),
        }),
        None => logging,
    };
    let runtime = crate::cmd::sandbox::open_with(root.to_path_buf(), security, approver)?;
    let mut env: Vec<(String, String)> = spec
        .env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    env.extend(def.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    env.push((spec.home_env.to_string(), home.display().to_string()));
    runtime.set_overlay(PlanOverlay {
        binds: vec![
            OverlayBind {
                path: binary.clone(),
                writable: false,
                why: format!("the {} CLI", spec.binary),
            },
            OverlayBind {
                path: home.clone(),
                writable: true,
                why: format!("{name}'s private home for this job (holds its login)"),
            },
        ],
        env,
    });

    // The line to the foreman: an MCP server the CLI launches, relaying to a socket
    // this process serves (see `mcp`). Only when there is a foreman to talk to.
    let seqs = mcp::Seqs::default();
    let control = ControlDir::from_env();
    let _mcp_host = match &control {
        Some(c) => {
            let socket = home.join(mcp::SOCKET_NAME);
            let task = mcp::serve_host(&socket, c.clone(), seqs.clone())?;
            match def.kind {
                HarnessKind::Grok => {
                    let config = home.join("config.toml");
                    let mut toml = std::fs::read_to_string(&config).unwrap_or_default();
                    toml.push_str(&mcp::grok_config_section(&mcp::shim_in_sandbox(), &socket));
                    write_private(&config, toml.as_bytes())?;
                }
            }
            Some(AbortOnDrop(task))
        }
        None => None,
    };

    let baseline = snapshot_tree(root, &home);
    let workdir = runtime.paths().workdir;
    let stderr_log = home.join("stderr.log");
    let command = command_line(def, &binary, &brief, &workdir, &home, &stderr_log);
    ui.notice(&format!(
        "{name}: running {}{} ({})",
        spec.binary,
        def.model
            .as_deref()
            .map(|m| format!(" · {m}"))
            .unwrap_or_default(),
        match def.auth {
            AuthExposure::AuthFile => "login file only",
            AuthExposure::FullHome => "login + vendor config",
        }
    ));

    // Stream the CLI's stdout through the parser; watch for a stall meanwhile.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let cancel = tokio_util::sync::CancellationToken::new();
    let exec = runtime.exec_stream(&command, None, 0, cancel.clone(), tx);
    tokio::pin!(exec);
    let mut parser = Parser::new(def.kind);
    let mut buf = String::new();
    let mut last_activity = Instant::now();
    let stall = Duration::from_secs(u64::from(def.stall_minutes.max(1)) * 60);
    let mut next_stall_note = stall;
    let result = loop {
        tokio::select! {
            res = &mut exec => break res,
            Some(chunk) = rx.recv() => {
                last_activity = Instant::now();
                next_stall_note = stall;
                feed(&mut buf, &chunk, &mut parser, ui);
            }
            _ = tokio::time::sleep(Duration::from_secs(15)) => {
                let quiet = last_activity.elapsed();
                if quiet >= next_stall_note {
                    let text = format!(
                        "no output from {name} for {}m — it may be stalled. It is left \
                         running; stop it with the stop-subagents control if it does not \
                         recover.",
                        quiet.as_secs() / 60
                    );
                    ui.notice(&text);
                    if let Some(c) = &control {
                        let _ = c.write_note(&Note { seq: seqs.next_note(), text });
                    }
                    // Back off: remind at 2×, 4×, … the stall window.
                    next_stall_note = quiet + next_stall_note;
                }
            }
        }
    };
    while let Ok(chunk) = rx.try_recv() {
        feed(&mut buf, &chunk, &mut parser, ui);
    }
    if !buf.trim().is_empty() {
        let line = std::mem::take(&mut buf);
        parser.on_line(&line, ui);
    }

    // Write back a refreshed login before anything can fail the job.
    write_back_auth(&home, &host_auth, &seeded, spec.auth_file, ui);
    let stderr = std::fs::read_to_string(&stderr_log).unwrap_or_default();

    let (exec_result, _) = result.context("running the harness")?;
    let code = exec_result.exit_code;
    if code != 0 || !parser.saw_events() {
        bail!("{}", failure_reason(spec.binary, code, &stderr));
    }
    let answer = parser.answer();
    let changed = baseline
        .and_then(|base| snapshot_tree(root, &home).map(|end| (base, end)))
        .map(|(base, end)| diff_stat(root, &base, &end))
        .unwrap_or_else(|| "(could not measure)".to_string());
    Ok(format!(
        "{}\n\n---\nChanged in the workspace while {name} ran (measured by cowboy):\n{}",
        if answer.is_empty() {
            "(no answer text)"
        } else {
            answer.as_str()
        },
        if changed.trim().is_empty() {
            "nothing".to_string()
        } else {
            changed
        }
    ))
}

/// How long a forwarded network request waits for the person before it is denied.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Sends this job's `ask` decisions to the foreman (see `watch_turn_requests` on the
/// parent side), which asks its own approver — i.e. the user, in the TUI or web —
/// and writes the answer back. Fails closed: no answer in time is a deny.
struct ForwardingApprover {
    control: ControlDir,
    /// Logs decisions the policy made on its own, as a non-forwarding child would.
    logging: std::sync::Arc<crate::sandbox::policy::ChannelApprover>,
    seq: std::sync::atomic::AtomicU32,
}

#[async_trait::async_trait]
impl cowboy_gateway::Approver for ForwardingApprover {
    async fn ask(
        &self,
        attempt: &cowboy_core::netproto::NetworkAttempt,
        reason: Option<&str>,
    ) -> cowboy_core::netproto::Verdict {
        self.answer(attempt, reason).await.verdict
    }

    async fn answer(
        &self,
        attempt: &cowboy_core::netproto::NetworkAttempt,
        reason: Option<&str>,
    ) -> cowboy_gateway::Answer {
        use cowboy_core::netproto::Verdict;
        let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let deny = cowboy_gateway::Answer {
            verdict: Verdict::Deny,
            remember: false,
        };
        let ask = crate::agent::jobctl::ApprovalAsk {
            seq,
            attempt: attempt.clone(),
            reason: reason.map(str::to_string),
        };
        if self.control.write_approval(&ask).is_err() {
            return deny;
        }
        let deadline = Instant::now() + FORWARD_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(r) = self.control.read_approval_reply(seq) {
                return cowboy_gateway::Answer {
                    verdict: if r.allow {
                        Verdict::Allow
                    } else {
                        Verdict::Deny
                    },
                    remember: r.remember,
                };
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        deny
    }

    async fn event(
        &self,
        attempt: &cowboy_core::netproto::NetworkAttempt,
        verdict: cowboy_core::netproto::Verdict,
        reason: String,
    ) {
        self.logging.event(attempt, verdict, reason).await;
    }
}

/// Split streamed chunks into lines for the parser.
fn feed(buf: &mut String, chunk: &str, parser: &mut Parser, ui: &mut dyn AgentUi) {
    buf.push_str(chunk);
    while let Some(i) = buf.find('\n') {
        let line: String = buf.drain(..=i).collect();
        parser.on_line(&line, ui);
    }
}

/// The per-kind stream parser.
enum Parser {
    Grok(grok::GrokStream),
}

impl Parser {
    fn new(kind: HarnessKind) -> Self {
        match kind {
            HarnessKind::Grok => Parser::Grok(grok::GrokStream::default()),
        }
    }
    fn on_line(&mut self, line: &str, ui: &mut dyn AgentUi) {
        match self {
            Parser::Grok(s) => s.on_line(line, ui),
        }
    }
    fn saw_events(&self) -> bool {
        match self {
            Parser::Grok(s) => s.saw_events,
        }
    }
    fn answer(&self) -> String {
        match self {
            Parser::Grok(s) => s.answer(),
        }
    }
}

/// The command line, as one `sh -c` string (stderr to a file, so stdout is pure
/// NDJSON). The CLI's approvals are skipped: cowboy's sandbox is the boundary.
fn command_line(
    def: &HarnessDef,
    binary: &Path,
    brief: &Path,
    workdir: &str,
    home: &Path,
    stderr_log: &Path,
) -> String {
    let mut args: Vec<String> = vec![binary.display().to_string()];
    match def.kind {
        HarnessKind::Grok => {
            args.extend([
                "--prompt-file".into(),
                brief.display().to_string(),
                "--output-format".into(),
                "streaming-json".into(),
                "--always-approve".into(),
                "--cwd".into(),
                workdir.to_string(),
                // Its own leader, in the private home: never a host grok's socket.
                "--leader-socket".into(),
                home.join("leader.sock").display().to_string(),
            ]);
            if let Some(m) = &def.model {
                args.extend(["-m".into(), m.clone()]);
            }
        }
    }
    args.extend(def.extra_args.iter().cloned());
    let quoted: Vec<String> = args.iter().map(|a| shell_quote(a)).collect();
    format!(
        "{} 2>{}",
        quoted.join(" "),
        shell_quote(&stderr_log.display().to_string())
    )
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// The CLI binary: the configured path, else the kind's name on `PATH`, with
/// symlinks resolved — the sandbox gets the real file, not a link into a directory
/// it cannot see.
fn resolve_binary(def: &HarnessDef, spec: &KindSpec) -> Result<PathBuf> {
    let found = match &def.binary {
        Some(p) => cowboy_core::config::expand_path(&p.to_string_lossy())
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        None => std::env::var_os("PATH")
            .into_iter()
            .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .map(|d| d.join(spec.binary))
            .find(|p| p.is_file())
            .with_context(|| format!("`{}` not found on PATH", spec.binary))?,
    };
    std::fs::canonicalize(&found).with_context(|| format!("resolving {}", found.display()))
}

/// `$XDG_STATE_HOME/cowboy/harness/<session>` (else `~/.local/state/…`), created
/// owner-only. Host-side and outside the workspace, so the foreman never sees it.
fn private_home(session_id: &str) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|s| !s.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME").context("no HOME")?).join(".local/state"),
    };
    let safe: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let dir = base.join("cowboy/harness").join(safe);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).ok();
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// Stops a background task when the job ends.
struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Removes the private home when the job ends, however it ends.
struct RemoveOnDrop(PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Copy the login (and with `full_home`, the vendor config) into the private
/// home. Returns the login's bytes as copied, to detect a refresh afterwards.
fn seed_home(home: &Path, vendor: &Path, spec: &KindSpec, auth: AuthExposure) -> Result<Vec<u8>> {
    if auth == AuthExposure::FullHome {
        for entry in std::fs::read_dir(vendor)?.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if spec.home_skip.contains(&name.as_ref()) || name.ends_with(".lock") {
                continue;
            }
            copy_tree(&entry.path(), &home.join(&*name))?;
        }
    }
    let bytes = std::fs::read(vendor.join(spec.auth_file)).context("reading the login file")?;
    write_private(&home.join(spec.auth_file), &bytes)?;
    Ok(bytes)
}

/// Copy a file or directory tree. Symlinks are skipped: one could point anywhere on
/// the host, and following it would copy that target into the sandbox.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)?.flatten() {
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else if meta.is_file() {
        std::fs::copy(from, to)?;
    }
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    f.write_all(bytes)?;
    Ok(())
}

/// If the harness refreshed its login, save it back to the user's real login file
/// — unless the host's copy changed meanwhile (the user's own grok refreshed it),
/// in which case the host's newer login wins.
fn write_back_auth(
    home: &Path,
    host_auth: &Path,
    seeded: &[u8],
    auth_file: &str,
    ui: &mut dyn AgentUi,
) {
    let Ok(now) = std::fs::read(home.join(auth_file)) else {
        return;
    };
    if now == seeded || now.is_empty() {
        return;
    }
    let host_now = std::fs::read(host_auth).unwrap_or_default();
    if host_now != seeded {
        ui.notice("the harness refreshed its login, but the host's changed too; kept the host's");
        return;
    }
    let tmp = host_auth.with_extension("json.cowboy-tmp");
    if write_private(&tmp, &now).is_ok() && std::fs::rename(&tmp, host_auth).is_ok() {
        ui.notice("saved the harness's refreshed login back to the host");
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// A tree object of the whole working tree (tracked + untracked, `.gitignore`
/// respected), without touching the real index: a copy of it is used so only
/// changed files are re-hashed. `None` outside a git repo.
fn snapshot_tree(root: &Path, scratch: &Path) -> Option<String> {
    let git = |args: &[&str], index: Option<&Path>| {
        let mut c = std::process::Command::new("git");
        c.arg("-C").arg(root).args(args);
        if let Some(i) = index {
            c.env("GIT_INDEX_FILE", i);
        }
        c.stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let real_index = git(&["rev-parse", "--git-path", "index"], None)?;
    let real_index = if Path::new(&real_index).is_absolute() {
        PathBuf::from(real_index)
    } else {
        root.join(real_index)
    };
    let tmp_index = scratch.join("cowboy-snapshot.index");
    let _ = std::fs::copy(&real_index, &tmp_index);
    git(&["add", "-A"], Some(&tmp_index))?;
    let tree = git(&["write-tree"], Some(&tmp_index));
    let _ = std::fs::remove_file(&tmp_index);
    tree
}

/// `git diff --stat` between two snapshot trees.
fn diff_stat(root: &Path, base: &str, end: &str) -> String {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--stat", base, end])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
        .unwrap_or_default()
}

/// Why a harness run failed, in words the foreman (and user) can act on.
fn failure_reason(cli: &str, code: i32, stderr: &str) -> String {
    let lower = stderr.to_lowercase();
    let tail: String = stderr
        .lines()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    let why = if [
        "401",
        "unauthorized",
        "unauthenticated",
        "not logged in",
        "login",
    ]
    .iter()
    .any(|k| lower.contains(k))
    {
        format!("{cli} is not logged in or its login expired — run `{cli} login` on the host")
    } else if [
        "429",
        "rate limit",
        "quota",
        "subscription",
        "too many requests",
        "usage limit",
    ]
    .iter()
    .any(|k| lower.contains(k))
    {
        format!("{cli} hit a subscription or rate limit")
    } else if code == 0 {
        format!("{cli} produced no output cowboy understands (a changed output format?)")
    } else {
        format!("{cli} exited with status {code}")
    };
    if tail.trim().is_empty() {
        why
    } else {
        format!("{why}\n{tail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def() -> HarnessDef {
        HarnessesConfig::parse(
            "harnesses:\n  grok:\n    kind: grok\n    model: grok-4.7\n    extra_args: [\"--no-plan\"]\n",
            Path::new("harnesses.yaml"),
        )
        .unwrap()
        .harnesses
        .remove("grok")
        .unwrap()
    }

    #[test]
    fn the_grok_command_skips_its_approvals_and_uses_a_private_leader() {
        let c = command_line(
            &def(),
            Path::new("/opt/grok"),
            Path::new("/state/h/cowboy-brief.md"),
            "/workspace",
            Path::new("/state/h"),
            Path::new("/state/h/stderr.log"),
        );
        assert!(
            c.starts_with("/opt/grok --prompt-file /state/h/cowboy-brief.md"),
            "{c}"
        );
        for part in [
            "--output-format streaming-json",
            "--always-approve",
            "--cwd /workspace",
            "--leader-socket /state/h/leader.sock",
            "-m grok-4.7",
            "--no-plan",
            "2>/state/h/stderr.log",
        ] {
            assert!(c.contains(part), "missing {part:?} in {c}");
        }
    }

    #[test]
    fn quoting_survives_spaces_and_quotes() {
        assert_eq!(shell_quote("plain-arg"), "plain-arg");
        assert_eq!(shell_quote("it's here"), "'it'\\''s here'");
    }

    #[test]
    fn failures_are_classified_for_the_foreman() {
        assert!(failure_reason("grok", 1, "HTTP 401 Unauthorized").contains("not logged in"));
        assert!(failure_reason("grok", 1, "error: 429 Too Many Requests").contains("limit"));
        assert!(failure_reason("grok", 0, "").contains("no output"));
        assert!(failure_reason("grok", 7, "boom").starts_with("grok exited with status 7"));
    }

    /// `full_home` copies config and credentials but not binaries, logs, sessions
    /// or symlinks; `auth_file` copies the login alone.
    #[test]
    fn seeding_copies_only_what_the_mode_allows() {
        let vendor = assert_fs::TempDir::new().unwrap();
        let v = vendor.path();
        std::fs::write(v.join("auth.json"), b"{\"t\":1}").unwrap();
        std::fs::write(v.join("config.toml"), b"x=1").unwrap();
        std::fs::create_dir_all(v.join("bin")).unwrap();
        std::fs::write(v.join("bin/grok"), b"elf").unwrap();
        std::fs::create_dir_all(v.join("sessions/a")).unwrap();
        std::fs::write(v.join("mcp_credentials.json"), b"{}").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", v.join("escape")).unwrap();
        let spec = HarnessKind::Grok.spec();

        let a = assert_fs::TempDir::new().unwrap();
        seed_home(a.path(), v, &spec, AuthExposure::AuthFile).unwrap();
        let names = |d: &Path| {
            let mut n: Vec<String> = std::fs::read_dir(d)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            n.sort();
            n
        };
        assert_eq!(names(a.path()), vec!["auth.json"]);

        let f = assert_fs::TempDir::new().unwrap();
        seed_home(f.path(), v, &spec, AuthExposure::FullHome).unwrap();
        assert_eq!(
            names(f.path()),
            vec!["auth.json", "config.toml", "mcp_credentials.json"]
        );
    }

    /// A refreshed login goes back to the host — unless the host's changed too.
    #[test]
    fn a_refreshed_login_is_written_back_only_over_an_unchanged_host_copy() {
        struct Quiet;
        impl AgentUi for Quiet {
            fn model_delta(&mut self, _: &str) {}
            fn command_start(&mut self, _: &str) {}
            fn command_end(&mut self, _: i32, _: &str) {}
            fn final_message(&mut self, _: &str) {}
            fn ask_user(&mut self, _: &str, _: &[String]) -> String {
                String::new()
            }
            fn notice(&mut self, _: &str) {}
        }
        let home = assert_fs::TempDir::new().unwrap();
        let host = assert_fs::TempDir::new().unwrap();
        let host_auth = host.path().join("auth.json");
        std::fs::write(&host_auth, b"old").unwrap();
        std::fs::write(home.path().join("auth.json"), b"new").unwrap();
        write_back_auth(home.path(), &host_auth, b"old", "auth.json", &mut Quiet);
        assert_eq!(std::fs::read(&host_auth).unwrap(), b"new");

        std::fs::write(&host_auth, b"host-refreshed").unwrap();
        std::fs::write(home.path().join("auth.json"), b"newer").unwrap();
        write_back_auth(home.path(), &host_auth, b"new", "auth.json", &mut Quiet);
        assert_eq!(std::fs::read(&host_auth).unwrap(), b"host-refreshed");
    }

    /// The measured change summary sees new, modified and deleted files and leaves
    /// the real index alone.
    #[test]
    fn the_change_summary_is_measured_from_the_working_tree() {
        let repo = assert_fs::TempDir::new().unwrap();
        let r = repo.path();
        let sh = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(r)
                .args(args)
                .output()
                .unwrap()
                .status
                .success());
        };
        sh(&["init", "-q"]);
        std::fs::write(r.join("a.txt"), "a\n").unwrap();
        sh(&["add", "."]);
        sh(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "i",
        ]);
        let scratch = assert_fs::TempDir::new().unwrap();
        let base = snapshot_tree(r, scratch.path()).unwrap();
        std::fs::write(r.join("a.txt"), "a\nb\n").unwrap();
        std::fs::write(r.join("new.txt"), "n\n").unwrap();
        let end = snapshot_tree(r, scratch.path()).unwrap();
        let stat = diff_stat(r, &base, &end);
        assert!(stat.contains("a.txt") && stat.contains("new.txt"), "{stat}");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(r)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&status.stdout).contains("?? new.txt"),
            "the real index must not have been touched"
        );
    }
}
