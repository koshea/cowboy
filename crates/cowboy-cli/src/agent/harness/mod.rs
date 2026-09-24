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

pub mod agy;
pub mod claude;
pub mod codex;
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
    let logged_in = host_home()
        .map(|h| h.join(spec.auth_files[0]).is_file())
        .unwrap_or(false);
    Inspection {
        binary,
        version,
        logged_in,
    }
}

/// The user's real home directory, where the vendor logins live.
fn host_home() -> Result<PathBuf> {
    cowboy_core::config::expand_path("~").map_err(|e| anyhow::anyhow!("{e}"))
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
    let install = install_root(&binary, spec.install_levels);
    let host = host_home()?;
    if !host.join(spec.auth_files[0]).is_file() {
        bail!(
            "not logged in — run `{}` and log in on the host first (no ~/{})",
            spec.binary,
            spec.auth_files[0]
        );
    }

    // The job's private home — the harness's HOME — host-only, removed when the
    // job ends. The login lands at the same ~-relative path the CLI looks in.
    let home = private_home(session_id)?;
    let _cleanup = RemoveOnDrop(home.clone());
    let seeded = seed_home(&home, &host, &spec, def.auth)?;
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
    // Last, so it wins over the sandbox's own HOME.
    env.push(("HOME".to_string(), home.display().to_string()));
    runtime.set_overlay(PlanOverlay {
        binds: vec![
            OverlayBind {
                path: install.clone(),
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
    let (_mcp_host, relay) = match &control {
        Some(c) => {
            let socket = home.join(mcp::SOCKET_NAME);
            let task = mcp::serve_host(&socket, c.clone(), seqs.clone())?;
            let relay = mcp::Relay {
                shim: mcp::shim_in_sandbox(),
                socket,
            };
            if def.kind == HarnessKind::Grok {
                // grok reads MCP servers from its config file.
                let config = home.join(".grok/config.toml");
                if let Some(dir) = config.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                let mut toml = std::fs::read_to_string(&config).unwrap_or_default();
                toml.push_str(&mcp::grok_config_section(&relay.shim, &relay.socket));
                write_private(&config, toml.as_bytes())?;
            }
            (Some(AbortOnDrop(task)), Some(relay))
        }
        None => (None, None),
    };

    let baseline = snapshot_tree(root, &home);
    let workdir = runtime.paths().workdir;
    let job = JobFiles {
        brief: brief.clone(),
        stderr: home.join("stderr.log"),
        last_message: home.join("last-message.txt"),
        leader_socket: home.join("leader.sock"),
    };
    let command = command_line(def, &binary, &job, &workdir, relay.as_ref());
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
    let mut parser = parser_for(def.kind);
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
                feed(&mut buf, &chunk, parser.as_mut(), ui);
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
        feed(&mut buf, &chunk, parser.as_mut(), ui);
    }
    if !buf.trim().is_empty() {
        let line = std::mem::take(&mut buf);
        parser.on_line(&line, ui);
    }

    // Write back a refreshed login before anything can fail the job.
    write_back_auth(&home, &host, &seeded, ui);
    let stderr = std::fs::read_to_string(&job.stderr).unwrap_or_default();

    let (exec_result, _) = result.context("running the harness")?;
    let code = exec_result.exit_code;
    if code != 0 || !parser.saw_events() || parser.error().is_some() {
        bail!(
            "{}",
            failure_reason(spec.binary, code, parser.error().as_deref(), &stderr)
        );
    }
    // codex writes its final message to a file as well; that copy is authoritative.
    let answer = std::fs::read_to_string(&job.last_message)
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| parser.answer());
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
fn feed(buf: &mut String, chunk: &str, parser: &mut dyn StreamParser, ui: &mut dyn AgentUi) {
    buf.push_str(chunk);
    while let Some(i) = buf.find('\n') {
        let line: String = buf.drain(..=i).collect();
        parser.on_line(&line, ui);
    }
}

/// A CLI's headless output stream, turned into cowboy UI events as it arrives.
pub trait StreamParser {
    /// Handle one line. Anything unrecognised is ignored, never fatal: these CLIs
    /// add event types between releases.
    fn on_line(&mut self, line: &str, ui: &mut dyn AgentUi);
    /// Whether any line was understood — "nothing we can read" (a flag or format
    /// change) is a failure, not an empty answer.
    fn saw_events(&self) -> bool;
    /// The run's final answer.
    fn answer(&self) -> String;
    /// An error the CLI reported in-band (a usage limit, a failed turn).
    fn error(&self) -> Option<String>;
}

fn parser_for(kind: HarnessKind) -> Box<dyn StreamParser> {
    match kind {
        HarnessKind::Grok => Box::new(grok::GrokStream::default()),
        HarnessKind::Claude => Box::new(claude::ClaudeStream::default()),
        HarnessKind::Codex => Box::new(codex::CodexStream::default()),
        HarnessKind::Agy => Box::new(agy::AgyStream::default()),
    }
}

/// Files of one job inside its private home.
struct JobFiles {
    brief: PathBuf,
    stderr: PathBuf,
    last_message: PathBuf,
    leader_socket: PathBuf,
}

/// One word of a command line: literal, or the brief's contents substituted by
/// the shell (for a CLI that only takes the prompt as an argument).
enum Word {
    Lit(String),
    BriefContents,
}

/// The command line, as one `sh -c` string: stdout is the CLI's NDJSON stream,
/// stderr goes to a file, stdin is the brief (or nothing). Every CLI's own
/// approvals and sandbox are off — cowboy's sandbox is the boundary.
fn command_line(
    def: &HarnessDef,
    binary: &Path,
    job: &JobFiles,
    workdir: &str,
    relay: Option<&mcp::Relay>,
) -> String {
    use Word::{BriefContents, Lit};
    let lit = |s: &str| Lit(s.to_string());
    let path = |p: &Path| Lit(p.display().to_string());
    let bin = path(binary);
    let mut pre: Vec<Vec<Word>> = Vec::new();
    // Where stdin comes from: the brief, or /dev/null (claude waits for stdin
    // otherwise).
    let mut stdin_brief = false;
    let mut words: Vec<Word> = vec![bin];
    match def.kind {
        HarnessKind::Grok => {
            words.extend([
                lit("--prompt-file"),
                path(&job.brief),
                lit("--output-format"),
                lit("streaming-json"),
                lit("--always-approve"),
                lit("--cwd"),
                lit(workdir),
                // Its own leader, in the private home: never a host grok's socket.
                lit("--leader-socket"),
                path(&job.leader_socket),
            ]);
            if let Some(m) = &def.model {
                words.extend([lit("-m"), lit(m)]);
            }
        }
        HarnessKind::Claude => {
            // `-p` with no prompt argument reads the prompt from stdin.
            stdin_brief = true;
            words.extend([
                lit("-p"),
                lit("--output-format"),
                lit("stream-json"),
                lit("--verbose"),
                lit("--permission-mode"),
                lit("bypassPermissions"),
            ]);
            if let Some(m) = &def.model {
                words.extend([lit("--model"), lit(m)]);
            }
            if let Some(r) = relay {
                words.extend([lit("--mcp-config"), lit(&r.claude_config())]);
            }
        }
        HarnessKind::Codex => {
            stdin_brief = true;
            words.extend([
                lit("exec"),
                lit("--json"),
                lit("--skip-git-repo-check"),
                lit("--dangerously-bypass-approvals-and-sandbox"),
                lit("-C"),
                lit(workdir),
                lit("-o"),
                path(&job.last_message),
            ]);
            if let Some(m) = &def.model {
                words.extend([lit("-m"), lit(m)]);
            }
            if let Some(r) = relay {
                for c in r.codex_overrides() {
                    words.extend([lit("-c"), Lit(c)]);
                }
            }
            // `-` = the prompt comes from stdin.
            words.push(lit("-"));
        }
        HarnessKind::Agy => {
            if let Some(r) = relay {
                // Registered in the private home's settings for this run only.
                let mut add = vec![
                    Lit(binary.display().to_string()),
                    lit("mcp"),
                    lit("add"),
                    lit("cowboy"),
                    lit("--"),
                ];
                add.extend(r.command_words().into_iter().map(Lit));
                pre.push(add);
            }
            words.extend([
                lit("-p"),
                BriefContents,
                lit("--output-format"),
                lit("stream-json"),
                // agy ignores the process cwd; without this it works in its own
                // scratch directory.
                lit("--add-dir"),
                lit(workdir),
                lit("--dangerously-skip-permissions"),
                lit("--disable-slash-commands"),
            ]);
            if let Some(m) = &def.model {
                words.extend([lit("--model"), lit(m)]);
            }
        }
    }
    words.extend(def.extra_args.iter().map(|a| lit(a)));
    let render = |ws: &[Word]| -> String {
        ws.iter()
            .map(|w| match w {
                Lit(s) => shell_quote(s),
                BriefContents => format!(
                    "\"$(cat {})\"",
                    shell_quote(&job.brief.display().to_string())
                ),
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    let stderr = shell_quote(&job.stderr.display().to_string());
    let stdin = if stdin_brief {
        shell_quote(&job.brief.display().to_string())
    } else {
        "/dev/null".to_string()
    };
    let mut script = String::new();
    for p in &pre {
        script.push_str(&format!(
            "{} </dev/null >/dev/null 2>>{stderr}; ",
            render(p)
        ));
    }
    script.push_str(&format!("{} <{stdin} 2>>{stderr}", render(&words)));
    script
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

/// What of the install the sandbox sees: the binary, or the directory `levels`
/// above it for a CLI that runs sibling helpers.
fn install_root(binary: &Path, levels: usize) -> PathBuf {
    let mut p = binary.to_path_buf();
    for _ in 0..levels {
        if let Some(parent) = p.parent() {
            p = parent.to_path_buf();
        }
    }
    p
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
    let parent = base.join("cowboy/harness");
    sweep_stale_homes(&parent);
    let dir = parent.join(safe);
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

/// How old a leftover private home must be before a later job removes it.
const STALE_HOME: Duration = Duration::from_secs(24 * 60 * 60);

/// Remove private homes a killed or crashed job left behind — each holds a copy of
/// a vendor login, which must not outlive its job. Anything younger than
/// [`STALE_HOME`] may belong to a job still running and is left alone.
fn sweep_stale_homes(parent: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE_HOME);
        if old {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Removes the private home when the job ends, however it ends.
struct RemoveOnDrop(PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Copy the login files (and with `full_home`, the vendor config dirs) from the
/// user's home into the private home, at the same `~`-relative paths. Returns what
/// was copied for each login file, to detect a refresh afterwards.
fn seed_home(
    home: &Path,
    host: &Path,
    spec: &KindSpec,
    auth: AuthExposure,
) -> Result<Vec<(String, Vec<u8>)>> {
    if auth == AuthExposure::FullHome {
        for dir in spec.vendor_dirs {
            let from = host.join(dir);
            let Ok(entries) = std::fs::read_dir(&from) else {
                continue;
            };
            let to = home.join(dir);
            std::fs::create_dir_all(&to)?;
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if spec.home_skip.contains(&name.as_ref()) || name.ends_with(".lock") {
                    continue;
                }
                copy_tree(&entry.path(), &to.join(&*name))?;
            }
        }
    }
    let mut seeded = Vec::new();
    for (i, rel) in spec.auth_files.iter().enumerate() {
        let bytes = match std::fs::read(host.join(rel)) {
            Ok(b) => b,
            // Only the first is the login; the rest are copied when present.
            Err(_) if i > 0 => continue,
            Err(e) => return Err(e).context("reading the login file"),
        };
        let dest = home.join(rel);
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if i == 0 {
            write_private(&dest, &bytes)?;
            // Only the credential is written back after the job.
            seeded.push((rel.to_string(), bytes));
        } else {
            let state = if auth == AuthExposure::AuthFile {
                strip_json_keys(&bytes, spec.strip_keys)
            } else {
                bytes
            };
            write_private(&dest, &state)?;
        }
    }
    Ok(seeded)
}

/// `bytes` as a JSON object without `keys`; unparseable input is dropped to `{}`
/// rather than passed through with whatever it holds.
fn strip_json_keys(bytes: &[u8], keys: &[&str]) -> Vec<u8> {
    if keys.is_empty() {
        return bytes.to_vec();
    }
    let mut v: serde_json::Value = serde_json::from_slice(bytes).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        for k in keys {
            o.remove(*k);
        }
    } else {
        v = serde_json::json!({});
    }
    serde_json::to_vec(&v).unwrap_or_default()
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

/// If the harness refreshed a login file, save it back to the user's real one —
/// unless the host's copy changed meanwhile (the user's own CLI refreshed it), in
/// which case the host's newer login wins.
fn write_back_auth(home: &Path, host: &Path, seeded: &[(String, Vec<u8>)], ui: &mut dyn AgentUi) {
    for (rel, before) in seeded {
        let Ok(now) = std::fs::read(home.join(rel)) else {
            continue;
        };
        if &now == before || now.is_empty() {
            continue;
        }
        let host_file = host.join(rel);
        if std::fs::read(&host_file).unwrap_or_default() != *before {
            ui.notice(&format!(
                "the harness refreshed ~/{rel}, but the host's changed too; kept the host's"
            ));
            continue;
        }
        let tmp = host_file.with_extension("cowboy-tmp");
        if write_private(&tmp, &now).is_ok() && std::fs::rename(&tmp, &host_file).is_ok() {
            ui.notice(&format!(
                "saved the harness's refreshed ~/{rel} back to the host"
            ));
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
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
fn failure_reason(cli: &str, code: i32, reported: Option<&str>, stderr: &str) -> String {
    let lower = format!("{}\n{stderr}", reported.unwrap_or_default()).to_lowercase();
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
    } else if let Some(r) = reported {
        format!("{cli} reported an error: {r}")
    } else if code == 0 {
        format!("{cli} produced no output cowboy understands (a changed output format?)")
    } else {
        format!("{cli} exited with status {code}")
    };
    // The CLI's own words carry what the classification cannot (when a limit
    // resets, which account) — keep them.
    let mut out = why;
    if let Some(r) = reported.filter(|r| !out.contains(r)) {
        out.push_str(&format!("\n{r}"));
    }
    if !tail.trim().is_empty() {
        out.push_str(&format!("\n{tail}"));
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Records the UI calls a stream parser makes.
    #[derive(Default)]
    pub(crate) struct Rec {
        pub deltas: String,
        pub commands: Vec<(String, i32)>,
        pub tools: Vec<String>,
        pub diffs: Vec<String>,
        pub cost: Option<f64>,
        pub tokens: Option<(u64, u64)>,
    }
    impl AgentUi for Rec {
        fn model_delta(&mut self, t: &str) {
            self.deltas.push_str(t);
        }
        fn command_start(&mut self, c: &str) {
            self.commands.push((c.to_string(), -1));
        }
        fn command_end(&mut self, code: i32, _o: &str) {
            if let Some(last) = self.commands.last_mut() {
                last.1 = code;
            }
        }
        fn tool_use(&mut self, s: &str) {
            self.tools.push(s.to_string());
        }
        fn file_diff(&mut self, p: &str, _d: &str) {
            self.diffs.push(p.to_string());
        }
        fn tokens(&mut self, i: u64, o: u64) {
            self.tokens = Some((i, o));
        }
        fn cost(&mut self, c: f64) {
            self.cost = Some(c);
        }
        fn final_message(&mut self, _m: &str) {}
        fn ask_user(&mut self, _q: &str, _o: &[String]) -> String {
            String::new()
        }
        fn notice(&mut self, _m: &str) {}
    }

    fn def(kind: &str, extra: &str) -> HarnessDef {
        HarnessesConfig::parse(
            &format!("harnesses:\n  h:\n    kind: {kind}\n    model: m-1\n{extra}"),
            Path::new("harnesses.yaml"),
        )
        .unwrap()
        .harnesses
        .remove("h")
        .unwrap()
    }

    fn job() -> JobFiles {
        JobFiles {
            brief: "/s/h/cowboy-brief.md".into(),
            stderr: "/s/h/stderr.log".into(),
            last_message: "/s/h/last-message.txt".into(),
            leader_socket: "/s/h/leader.sock".into(),
        }
    }

    fn relay() -> mcp::Relay {
        mcp::Relay {
            shim: "/.cowboy-shim".into(),
            socket: "/s/h/cowboy-foreman.sock".into(),
        }
    }

    fn has(c: &str, parts: &[&str]) {
        for p in parts {
            assert!(c.contains(p), "missing {p:?} in {c}");
        }
    }

    #[test]
    fn grok_skips_its_approvals_and_uses_a_private_leader() {
        let c = command_line(
            &def("grok", "    extra_args: [\"--no-plan\"]\n"),
            Path::new("/opt/grok"),
            &job(),
            "/workspace",
            Some(&relay()),
        );
        assert!(
            c.starts_with("/opt/grok --prompt-file /s/h/cowboy-brief.md"),
            "{c}"
        );
        has(
            &c,
            &[
                "--output-format streaming-json",
                "--always-approve",
                "--cwd /workspace",
                "--leader-socket /s/h/leader.sock",
                "-m m-1",
                "--no-plan",
                "</dev/null",
                "2>>/s/h/stderr.log",
            ],
        );
    }

    #[test]
    fn claude_reads_the_brief_on_stdin_and_gets_the_relay_as_mcp_config() {
        let c = command_line(
            &def("claude", ""),
            Path::new("/opt/claude"),
            &job(),
            "/w",
            Some(&relay()),
        );
        has(
            &c,
            &[
                "-p --output-format stream-json --verbose",
                "--permission-mode bypassPermissions",
                "--model m-1",
                "--mcp-config",
                "x-foreman-mcp",
                "</s/h/cowboy-brief.md",
            ],
        );
    }

    #[test]
    fn codex_runs_exec_in_the_workdir_and_writes_its_last_message() {
        let c = command_line(
            &def("codex", ""),
            Path::new("/opt/codex"),
            &job(),
            "/w",
            Some(&relay()),
        );
        has(
            &c,
            &[
                "/opt/codex exec --json --skip-git-repo-check",
                "--dangerously-bypass-approvals-and-sandbox",
                "-C /w",
                "-o /s/h/last-message.txt",
                "-m m-1",
                "mcp_servers.cowboy.command=",
                "</s/h/cowboy-brief.md",
            ],
        );
        assert!(c.contains(" - <"), "the prompt comes from stdin: {c}");
    }

    #[test]
    fn agy_registers_the_relay_then_runs_on_the_workdir() {
        let c = command_line(
            &def("agy", ""),
            Path::new("/opt/agy"),
            &job(),
            "/w",
            Some(&relay()),
        );
        let (setup, run) = c.split_once("; ").expect("mcp add, then the run");
        has(
            setup,
            &["/opt/agy mcp add cowboy -- /.cowboy-shim x-foreman-mcp"],
        );
        has(
            run,
            &[
                "-p \"$(cat /s/h/cowboy-brief.md)\"",
                "--output-format stream-json",
                "--add-dir /w",
                "--dangerously-skip-permissions",
                "--disable-slash-commands",
                "--model m-1",
                "</dev/null",
            ],
        );
        // No foreman, no relay.
        let c = command_line(&def("agy", ""), Path::new("/opt/agy"), &job(), "/w", None);
        assert!(!c.contains("mcp add"), "{c}");
    }

    #[test]
    fn quoting_survives_spaces_and_quotes() {
        assert_eq!(shell_quote("plain-arg"), "plain-arg");
        assert_eq!(shell_quote("it's here"), "'it'\\''s here'");
    }

    #[test]
    fn failures_are_classified_for_the_foreman() {
        let f = |code, reported, stderr| failure_reason("x", code, reported, stderr);
        assert!(f(1, None, "HTTP 401 Unauthorized").contains("not logged in"));
        assert!(f(1, None, "error: 429 Too Many Requests").contains("limit"));
        let limit = f(
            1,
            Some("You've hit your usage limit. Try again at 9:38 AM."),
            "",
        );
        assert!(
            limit.contains("hit a subscription or rate limit"),
            "{limit}"
        );
        assert!(
            limit.contains("9:38 AM"),
            "the CLI's own words are kept: {limit}"
        );
        assert!(f(0, Some("status CANCELLED"), "").contains("reported an error"));
        assert!(f(0, None, "").contains("no output"));
        assert!(f(7, None, "boom").starts_with("x exited with status 7"));
    }

    fn names(d: &Path) -> Vec<String> {
        let mut n: Vec<String> = std::fs::read_dir(d)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        n.sort();
        n
    }

    /// `full_home` copies config and credentials but not binaries, logs, sessions
    /// or symlinks; `auth_file` copies the login alone, at its ~-relative path.
    #[test]
    fn seeding_copies_only_what_the_mode_allows() {
        let host = assert_fs::TempDir::new().unwrap();
        let v = host.path().join(".grok");
        std::fs::create_dir_all(v.join("bin")).unwrap();
        std::fs::write(v.join("auth.json"), b"{\"t\":1}").unwrap();
        std::fs::write(v.join("config.toml"), b"x=1").unwrap();
        std::fs::write(v.join("bin/grok"), b"elf").unwrap();
        std::fs::create_dir_all(v.join("sessions/a")).unwrap();
        std::fs::write(v.join("mcp_credentials.json"), b"{}").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", v.join("escape")).unwrap();
        let spec = HarnessKind::Grok.spec();

        let a = assert_fs::TempDir::new().unwrap();
        seed_home(a.path(), host.path(), &spec, AuthExposure::AuthFile).unwrap();
        assert_eq!(names(&a.path().join(".grok")), vec!["auth.json"]);

        let f = assert_fs::TempDir::new().unwrap();
        seed_home(f.path(), host.path(), &spec, AuthExposure::FullHome).unwrap();
        assert_eq!(
            names(&f.path().join(".grok")),
            vec!["auth.json", "config.toml", "mcp_credentials.json"]
        );
    }

    /// Claude's login is two files, one outside its config dir; the second is
    /// optional. A missing first file is "not logged in".
    #[test]
    fn claude_login_is_both_files_when_present() {
        let host = assert_fs::TempDir::new().unwrap();
        std::fs::create_dir_all(host.path().join(".claude")).unwrap();
        std::fs::write(host.path().join(".claude/.credentials.json"), b"c").unwrap();
        let spec = HarnessKind::Claude.spec();
        std::fs::write(
            host.path().join(".claude.json"),
            br#"{"oauthAccount":{"x":1},"mcpServers":{"s":{"command":"uvx"}},"projects":{}}"#,
        )
        .unwrap();
        let home = assert_fs::TempDir::new().unwrap();
        let seeded = seed_home(home.path(), host.path(), &spec, AuthExposure::AuthFile).unwrap();
        assert_eq!(seeded.len(), 1, "only the credential is written back");
        assert!(home.path().join(".claude/.credentials.json").is_file());
        let state = std::fs::read_to_string(home.path().join(".claude.json")).unwrap();
        assert!(state.contains("oauthAccount"), "{state}");
        assert!(
            !state.contains("mcpServers") && !state.contains("projects"),
            "the user's MCP servers must not start in the sandbox: {state}"
        );

        std::fs::remove_file(host.path().join(".claude.json")).unwrap();
        let home = assert_fs::TempDir::new().unwrap();
        seed_home(home.path(), host.path(), &spec, AuthExposure::AuthFile).unwrap();
        assert!(!home.path().join(".claude.json").exists());
        std::fs::remove_file(host.path().join(".claude/.credentials.json")).unwrap();
        assert!(seed_home(home.path(), host.path(), &spec, AuthExposure::AuthFile).is_err());
    }

    /// A refreshed login goes back to the host — unless the host's changed too.
    #[test]
    fn a_refreshed_login_is_written_back_only_over_an_unchanged_host_copy() {
        let home = assert_fs::TempDir::new().unwrap();
        let host = assert_fs::TempDir::new().unwrap();
        let rel = ".grok/auth.json".to_string();
        for d in [home.path(), host.path()] {
            std::fs::create_dir_all(d.join(".grok")).unwrap();
        }
        std::fs::write(host.path().join(&rel), b"old").unwrap();
        std::fs::write(home.path().join(&rel), b"new").unwrap();
        write_back_auth(
            home.path(),
            host.path(),
            &[(rel.clone(), b"old".to_vec())],
            &mut Rec::default(),
        );
        assert_eq!(std::fs::read(host.path().join(&rel)).unwrap(), b"new");

        std::fs::write(host.path().join(&rel), b"host-refreshed").unwrap();
        std::fs::write(home.path().join(&rel), b"newer").unwrap();
        write_back_auth(
            home.path(),
            host.path(),
            &[(rel.clone(), b"new".to_vec())],
            &mut Rec::default(),
        );
        assert_eq!(
            std::fs::read(host.path().join(&rel)).unwrap(),
            b"host-refreshed"
        );
    }

    /// A leftover home (a killed job's, holding a login copy) is swept once stale;
    /// a fresh one — possibly a running job's — is kept.
    #[test]
    fn stale_private_homes_are_swept() {
        let parent = assert_fs::TempDir::new().unwrap();
        let old = parent.path().join("old-job");
        let fresh = parent.path().join("fresh-job");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&fresh).unwrap();
        let past = std::time::SystemTime::now() - STALE_HOME - Duration::from_secs(60);
        std::fs::File::open(&old)
            .unwrap()
            .set_modified(past)
            .unwrap();
        sweep_stale_homes(parent.path());
        assert!(!old.exists() && fresh.exists());
    }

    #[test]
    fn codex_gets_its_whole_release_directory() {
        let b = Path::new("/h/.codex/packages/standalone/releases/0.154.0/bin/codex");
        assert_eq!(
            install_root(b, HarnessKind::Codex.spec().install_levels),
            Path::new("/h/.codex/packages/standalone/releases/0.154.0")
        );
        assert_eq!(install_root(Path::new("/x/grok"), 0), Path::new("/x/grok"));
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
