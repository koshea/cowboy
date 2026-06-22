//! Session logging and replay.
//!
//! Each session writes a directory under `.cowboy/sessions/<id>/` containing
//! newline-delimited JSON logs (transcript, commands), a final summary, and a
//! saved diff. We own the schema, so records are hand-written `serde_json`
//! lines (one object per line) for stable replay/diffing.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use cowboy_core::model::Message;
use serde::{Deserialize, Serialize};

pub mod replay;

/// Writes the artifacts for one session.
pub struct SessionLogger {
    id: String,
    root: PathBuf,
    dir: PathBuf,
    transcript: File,
    commands: File,
    command_seq: u32,
    message_seq: u32,
}

/// A logged command record (`commands.jsonl`).
#[derive(Debug, Serialize, Deserialize)]
pub struct CommandRecord {
    pub seq: u32,
    pub ts_ms: u128,
    pub command: String,
    pub exit_code: i32,
    pub duration_ms: u128,
    pub output_bytes: usize,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

impl SessionLogger {
    /// Create a new session directory under `root/.cowboy/sessions/`. Honors a
    /// caller-supplied `COWBOY_SESSION_ID` (a subagent parent assigns the child's
    /// id so it knows where the child's journal lives), else generates one.
    pub fn create(root: &Path) -> Result<Self> {
        let id = std::env::var("COWBOY_SESSION_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("{}-{}", now_ms(), std::process::id()));
        Self::create_with_id(root, &id)
    }

    /// Create a session with a caller-supplied id (used by the daemon-managed
    /// worker so the registry id and the session dir agree).
    pub fn create_with_id(root: &Path, id: &str) -> Result<Self> {
        let id = id.to_string();
        let dir = session_dir(root, &id);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating session dir {}", dir.display()))?;
        // Maintain a `current` symlink-like pointer file for convenience.
        let _ = std::fs::write(sessions_dir(root).join("LATEST"), &id);
        let transcript = create_file(&dir.join("transcript.jsonl"))?;
        let commands = create_file(&dir.join("commands.jsonl"))?;
        std::fs::create_dir_all(dir.join("commands")).ok();
        // Scratchpad the agent may write to (mounted, editable).
        let _ = std::fs::write(dir.join("scratchpad.md"), "");
        Ok(Self {
            id,
            root: root.to_path_buf(),
            dir,
            transcript,
            commands,
            command_seq: 0,
            message_seq: 0,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }
    /// The session directory (used by tests and diagnostics).
    #[allow(dead_code)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Append a conversation message to the transcript.
    pub fn log_message(&mut self, msg: &Message) {
        self.message_seq += 1;
        if let Ok(line) = serde_json::to_string(msg) {
            let _ = writeln!(self.transcript, "{line}");
        }
    }

    /// Append a command record and write its full (untruncated) output to a
    /// per-command file under `commands/`.
    pub fn log_command(&mut self, command: &str, exit_code: i32, duration_ms: u128, output: &str) {
        self.command_seq += 1;
        let out_path = self
            .dir
            .join("commands")
            .join(format!("{:04}.out", self.command_seq));
        let _ = std::fs::write(&out_path, output);
        let rec = CommandRecord {
            seq: self.command_seq,
            ts_ms: now_ms(),
            command: command.to_string(),
            exit_code,
            duration_ms,
            output_bytes: output.len(),
        };
        if let Ok(line) = serde_json::to_string(&rec) {
            let _ = writeln!(self.commands, "{line}");
        }
    }

    /// Write the final summary.
    pub fn write_final(&self, message: &str) {
        let _ = std::fs::write(self.dir.join("final.md"), message);
    }

    /// At session end: capture the workspace diff and a context summary.
    pub fn finalize(&self, final_message: Option<&str>) {
        // final.md — the agent's answer. The explicit `final` tool writes it
        // directly (via `write_final`), but a model that just answers in plain
        // text (no tool call) ends the session through the implicit-final path,
        // which never wrote the file. Backfill it from the final message so
        // every finished session has a non-empty final.md for downstream readers
        // (logs/replay/handoff, and a foreman reading a subagent result).
        if let Some(msg) = final_message {
            let final_path = self.dir.join("final.md");
            let empty = std::fs::metadata(&final_path)
                .map(|m| m.len() == 0)
                .unwrap_or(true);
            if empty {
                let _ = std::fs::write(&final_path, msg);
            }
        }
        // diff.patch — git diff of the workspace (best-effort).
        if let Ok(out) = std::process::Command::new("git")
            .arg("diff")
            .current_dir(&self.root)
            .output()
        {
            if out.status.success() {
                let _ = std::fs::write(self.dir.join("diff.patch"), &out.stdout);
            }
        }
        // context-summary.md — lightweight session stats.
        let summary = format!(
            "# Session {}\n\n- messages: {}\n- commands: {}\n- final: {}\n",
            self.id,
            self.message_seq,
            self.command_seq,
            final_message.unwrap_or("(none / interrupted)"),
        );
        let _ = std::fs::write(self.dir.join("context-summary.md"), summary);

        // handoff.md — every finished session has one. If the agent didn't write
        // a structured handoff via the tool, synthesize a minimal one so a
        // downstream worker/coordinator always has something to read.
        let handoff = self.dir.join("handoff.md");
        if !handoff.exists() {
            let body = format!(
                "# Handoff (auto-generated)\n\n## Status\n{}\n\n## Summary\n{}\n\n\
                 ## Activity\n- messages: {}\n- commands: {}\n- diff: see diff.patch\n",
                if final_message.is_some() {
                    "complete"
                } else {
                    "incomplete (interrupted)"
                },
                final_message.unwrap_or("(no final message)"),
                self.message_seq,
                self.command_seq,
            );
            let _ = std::fs::write(handoff, body);
        }
    }
}

/// The `.cowboy/sessions` directory for a project.
pub fn sessions_dir(root: &Path) -> PathBuf {
    root.join(cowboy_core::config::COWBOY_DIR).join("sessions")
}

/// The directory for a specific session (`.cowboy/sessions/<id>`). Single source
/// of truth for the on-disk session layout — readers must not rebuild this path
/// by hand.
pub fn session_dir(root: &Path, id: &str) -> PathBuf {
    sessions_dir(root).join(id)
}

/// The most recent session directory for a project, if any (via the `LATEST`
/// pointer). Used to attach out-of-session artifacts like `processes.jsonl`.
pub fn latest_session_dir(root: &Path) -> Option<PathBuf> {
    let dir = session_dir(root, latest_session_id(root)?.as_str());
    dir.is_dir().then_some(dir)
}

/// The id of the most recent session for a project (via the `LATEST` pointer),
/// if its directory still exists.
pub fn latest_session_id(root: &Path) -> Option<String> {
    let id = std::fs::read_to_string(sessions_dir(root).join("LATEST")).ok()?;
    let id = id.trim().to_string();
    session_dir(root, &id).is_dir().then_some(id)
}

/// Load a prior session's conversation transcript so a new session can continue
/// it. Drops any leading system message (the new session supplies its own
/// up-to-date system prompt) and trims a trailing, incomplete tool-call turn
/// (e.g. from a crashed session) so the history is valid to resume from.
pub fn load_history(root: &Path, id: &str) -> Result<Vec<Message>> {
    use std::io::BufRead;
    let path = session_dir(root, id).join("transcript.jsonl");
    let file = File::open(&path)
        .with_context(|| format!("opening transcript {} (no such session?)", path.display()))?;
    let mut msgs: Vec<Message> = Vec::new();
    for line in std::io::BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(m) = serde_json::from_str::<Message>(&line) {
            // The new session owns the system prompt; skip any saved one.
            if m.role == cowboy_core::model::Role::System {
                continue;
            }
            msgs.push(m);
        }
    }
    sanitize_history(&mut msgs);
    Ok(msgs)
}

/// Trim a trailing assistant tool-call turn whose tool results are missing or
/// incomplete (providers reject an unanswered tool call). Walks back from the
/// end to the last assistant message bearing tool calls and, if not every call
/// id has a following `Tool` result, drops that assistant and anything after it.
fn sanitize_history(msgs: &mut Vec<Message>) {
    use cowboy_core::model::Role;
    let Some(asst_idx) = msgs
        .iter()
        .rposition(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
    else {
        return;
    };
    let answered: std::collections::HashSet<&str> = msgs[asst_idx + 1..]
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    let complete = msgs[asst_idx]
        .tool_calls
        .iter()
        .all(|tc| answered.contains(tc.id.as_str()));
    if !complete {
        msgs.truncate(asst_idx);
    }
}

/// Append a value as one JSON line to `path` (best-effort; used by the control
/// pipeline for `network.jsonl` / `approvals.jsonl`, which run on other tasks).
pub fn append_jsonl<T: Serialize>(path: &Path, value: &T) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        if let Ok(line) = serde_json::to_string(value) {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn create_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::model::Message;

    #[test]
    fn writes_session_artifacts() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let mut log = SessionLogger::create(tmp.path()).unwrap();
        log.log_message(&Message::user("do the thing"));
        log.log_command("ls", 0, 12, "file1\nfile2\n");
        log.write_final("all done");

        let dir = log.dir().to_path_buf();
        let transcript = std::fs::read_to_string(dir.join("transcript.jsonl")).unwrap();
        assert!(transcript.contains("do the thing"));
        let commands = std::fs::read_to_string(dir.join("commands.jsonl")).unwrap();
        assert!(commands.contains("\"command\":\"ls\""));
        assert!(commands.contains("\"exit_code\":0"));
        // Per-command output file written under commands/.
        let cmd_out = std::fs::read_to_string(dir.join("commands/0001.out")).unwrap();
        assert_eq!(cmd_out, "file1\nfile2\n");
        let final_md = std::fs::read_to_string(dir.join("final.md")).unwrap();
        assert_eq!(final_md, "all done");

        // finalize writes a context summary.
        log.finalize(Some("all done"));
        assert!(dir.join("context-summary.md").exists());

        // LATEST points at this session.
        let latest = std::fs::read_to_string(tmp.path().join(".cowboy/sessions/LATEST")).unwrap();
        assert_eq!(latest, log.id());
    }

    use cowboy_core::model::{Role, ToolCall};

    #[test]
    fn finalize_synthesizes_handoff_when_none_written() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let log = SessionLogger::create(tmp.path()).unwrap();
        let dir = log.dir().to_path_buf();
        assert!(!dir.join("handoff.md").exists());
        log.finalize(Some("all done"));
        let md = std::fs::read_to_string(dir.join("handoff.md")).unwrap();
        assert!(md.contains("auto-generated"));
        assert!(md.contains("all done"));

        // A pre-existing handoff (e.g. from the tool) is not clobbered.
        std::fs::write(dir.join("handoff.md"), "AGENT HANDOFF").unwrap();
        log.finalize(Some("all done"));
        assert_eq!(
            std::fs::read_to_string(dir.join("handoff.md")).unwrap(),
            "AGENT HANDOFF"
        );
    }

    #[test]
    fn finalize_backfills_final_md_from_implicit_final() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let log = SessionLogger::create(tmp.path()).unwrap();
        let dir = log.dir().to_path_buf();
        // No `final` tool was called: final.md does not exist yet. A model that
        // answered in plain text reaches finalize with the message in hand.
        assert!(!dir.join("final.md").exists());
        log.finalize(Some("the plain-text answer"));
        assert_eq!(
            std::fs::read_to_string(dir.join("final.md")).unwrap(),
            "the plain-text answer"
        );

        // An explicit final.md (from the `final` tool) is not clobbered.
        let log2 = SessionLogger::create(tmp.path()).unwrap();
        let dir2 = log2.dir().to_path_buf();
        log2.write_final("tool-written final");
        log2.finalize(Some("different last message"));
        assert_eq!(
            std::fs::read_to_string(dir2.join("final.md")).unwrap(),
            "tool-written final"
        );
    }

    #[test]
    fn load_history_drops_system_and_resolves_latest() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let id = {
            let mut log = SessionLogger::create(tmp.path()).unwrap();
            log.log_message(&Message::system("OLD SYSTEM PROMPT"));
            log.log_message(&Message::user("first task"));
            log.log_message(&Message::new(Role::Assistant, "did it"));
            log.id().to_string()
        };

        assert_eq!(latest_session_id(tmp.path()).as_deref(), Some(id.as_str()));
        let hist = load_history(tmp.path(), &id).unwrap();
        // System prompt is dropped; the user+assistant turn is kept in order.
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].role, Role::User);
        assert_eq!(hist[0].content, "first task");
        assert_eq!(hist[1].role, Role::Assistant);
        assert!(!hist.iter().any(|m| m.role == Role::System));
    }

    #[test]
    fn sanitize_drops_trailing_incomplete_tool_turn() {
        // user, assistant(tool_calls a+b), tool(a)  — b is unanswered (crash).
        let mut msgs = vec![
            Message::user("go"),
            Message {
                reasoning: None,
                role: Role::Assistant,
                content: String::new(),
                tool_call_id: None,
                tool_calls: vec![
                    ToolCall {
                        id: "a".into(),
                        name: "shell".into(),
                        arguments: "{}".into(),
                    },
                    ToolCall {
                        id: "b".into(),
                        name: "shell".into(),
                        arguments: "{}".into(),
                    },
                ],
            },
            Message::tool_result("a", "ok"),
        ];
        sanitize_history(&mut msgs);
        // The incomplete assistant turn (and its partial result) is removed.
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, Role::User);
    }

    #[test]
    fn sanitize_keeps_complete_tool_turn() {
        let mut msgs = vec![
            Message::user("go"),
            Message {
                reasoning: None,
                role: Role::Assistant,
                content: String::new(),
                tool_call_id: None,
                tool_calls: vec![ToolCall {
                    id: "a".into(),
                    name: "shell".into(),
                    arguments: "{}".into(),
                }],
            },
            Message::tool_result("a", "ok"),
        ];
        let before = msgs.len();
        sanitize_history(&mut msgs);
        assert_eq!(msgs.len(), before);
    }
}
