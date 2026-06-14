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
    /// Create a new session directory under `root/.cowboy/sessions/`.
    pub fn create(root: &Path) -> Result<Self> {
        Self::create_with_id(root, &format!("{}-{}", now_ms(), std::process::id()))
    }

    /// Create a session with a caller-supplied id (used by the daemon-managed
    /// worker so the registry id and the session dir agree).
    pub fn create_with_id(root: &Path, id: &str) -> Result<Self> {
        let id = id.to_string();
        let dir = root.join(".cowboy").join("sessions").join(&id);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating session dir {}", dir.display()))?;
        // Maintain a `current` symlink-like pointer file for convenience.
        let _ = std::fs::write(root.join(".cowboy").join("sessions").join("LATEST"), &id);
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
    }
}

/// The most recent session directory for a project, if any (via the `LATEST`
/// pointer). Used to attach out-of-session artifacts like `processes.jsonl`.
pub fn latest_session_dir(root: &Path) -> Option<PathBuf> {
    let sessions = root.join(".cowboy").join("sessions");
    let id = std::fs::read_to_string(sessions.join("LATEST")).ok()?;
    let dir = sessions.join(id.trim());
    dir.is_dir().then_some(dir)
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
}
