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
    dir: PathBuf,
    transcript: File,
    commands: File,
    command_seq: u32,
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
        let id = format!("{}-{}", now_ms(), std::process::id());
        let dir = root.join(".cowboy").join("sessions").join(&id);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating session dir {}", dir.display()))?;
        // Maintain a `current` symlink-like pointer file for convenience.
        let _ = std::fs::write(root.join(".cowboy").join("sessions").join("LATEST"), &id);
        let transcript = create_file(&dir.join("transcript.jsonl"))?;
        let commands = create_file(&dir.join("commands.jsonl"))?;
        Ok(Self {
            id,
            dir,
            transcript,
            commands,
            command_seq: 0,
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
        if let Ok(line) = serde_json::to_string(msg) {
            let _ = writeln!(self.transcript, "{line}");
        }
    }

    /// Append a command record.
    pub fn log_command(
        &mut self,
        command: &str,
        exit_code: i32,
        duration_ms: u128,
        output_bytes: usize,
    ) {
        self.command_seq += 1;
        let rec = CommandRecord {
            seq: self.command_seq,
            ts_ms: now_ms(),
            command: command.to_string(),
            exit_code,
            duration_ms,
            output_bytes,
        };
        if let Ok(line) = serde_json::to_string(&rec) {
            let _ = writeln!(self.commands, "{line}");
        }
    }

    /// Write the final summary.
    pub fn write_final(&self, message: &str) {
        let _ = std::fs::write(self.dir.join("final.md"), message);
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
        log.log_command("ls", 0, 12, 42);
        log.write_final("all done");

        let dir = log.dir().to_path_buf();
        let transcript = std::fs::read_to_string(dir.join("transcript.jsonl")).unwrap();
        assert!(transcript.contains("do the thing"));
        let commands = std::fs::read_to_string(dir.join("commands.jsonl")).unwrap();
        assert!(commands.contains("\"command\":\"ls\""));
        assert!(commands.contains("\"exit_code\":0"));
        let final_md = std::fs::read_to_string(dir.join("final.md")).unwrap();
        assert_eq!(final_md, "all done");

        // LATEST points at this session.
        let latest = std::fs::read_to_string(tmp.path().join(".cowboy/sessions/LATEST")).unwrap();
        assert_eq!(latest, log.id());
    }
}
