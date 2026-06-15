//! Listing and read-only replay of past sessions.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use cowboy_core::model::{Message, Role};

use super::CommandRecord;

fn sessions_dir(root: &Path) -> PathBuf {
    root.join(".cowboy").join("sessions")
}

/// List sessions newest-first with a one-line summary.
pub fn list(root: &Path) -> Result<()> {
    let dir = sessions_dir(root);
    if !dir.exists() {
        println!("no sessions yet");
        return Ok(());
    }
    let mut ids: Vec<String> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    ids.sort();
    ids.reverse();

    if ids.is_empty() {
        println!("no sessions yet");
        return Ok(());
    }
    for id in ids {
        let final_path = dir.join(&id).join("final.md");
        let summary = std::fs::read_to_string(&final_path)
            .ok()
            .and_then(|s| s.lines().next().map(str::to_string))
            .unwrap_or_else(|| "(no final summary)".into());
        println!("{id}  {summary}");
    }
    Ok(())
}

/// Render a past session to stdout.
pub fn replay(root: &Path, id: &str) -> Result<()> {
    let dir = sessions_dir(root).join(id);
    if !dir.exists() {
        bail!("no such session: {id}");
    }

    println!("=== session {id} ===\n");

    let transcript = dir.join("transcript.jsonl");
    let text = std::fs::read_to_string(&transcript)
        .with_context(|| format!("reading {}", transcript.display()))?;
    for line in text.lines() {
        let Ok(msg) = serde_json::from_str::<Message>(line) else {
            continue;
        };
        match msg.role {
            Role::User => println!("\x1b[1myou:\x1b[0m {}", msg.content),
            Role::Assistant => {
                if !msg.content.is_empty() {
                    println!("\x1b[36magent:\x1b[0m {}", msg.content);
                }
                for tc in &msg.tool_calls {
                    println!("\x1b[33m  → {}({})\x1b[0m", tc.name, tc.arguments);
                }
            }
            Role::Tool => println!("\x1b[2m  ⤷ {}\x1b[0m", first_line(&msg.content)),
            Role::System => {}
        }
    }

    // Commands summary.
    let commands = dir.join("commands.jsonl");
    if let Ok(text) = std::fs::read_to_string(&commands) {
        let recs: Vec<CommandRecord> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if !recs.is_empty() {
            println!("\n--- commands ({}) ---", recs.len());
            for r in recs {
                println!(
                    "  [{}] exit={} {}ms  {}",
                    r.seq, r.exit_code, r.duration_ms, r.command
                );
            }
        }
    }

    // Other artifacts: line counts + diff presence.
    for (label, file) in [
        ("network decisions", "network.jsonl"),
        ("approvals", "approvals.jsonl"),
        ("process events", "processes.jsonl"),
    ] {
        if let Ok(text) = std::fs::read_to_string(dir.join(file)) {
            let n = text.lines().filter(|l| !l.trim().is_empty()).count();
            if n > 0 {
                println!("--- {label} ({n}) ---");
                for line in text.lines().take(20) {
                    println!("  {line}");
                }
            }
        }
    }
    let artifacts = cowboy_core::artifact::list_in(&dir);
    if !artifacts.is_empty() {
        println!("--- artifacts ({}) ---", artifacts.len());
        for a in &artifacts {
            println!("  {} [{}] {}", a.id, a.kind.as_str(), a.title);
        }
    }
    if let Ok(meta) = std::fs::metadata(dir.join("diff.patch")) {
        if meta.len() > 0 {
            println!("--- diff.patch ({} bytes) ---", meta.len());
        }
    }

    if let Ok(final_md) = std::fs::read_to_string(dir.join("final.md")) {
        println!("\n\x1b[1;32m✓ {final_md}\x1b[0m");
    }
    Ok(())
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionLogger;
    use cowboy_core::model::{Message, Role, ToolCall};

    #[test]
    fn replay_renders_a_recorded_session() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let mut log = SessionLogger::create(tmp.path()).unwrap();
        log.log_message(&Message::user("inspect repo"));
        log.log_message(&Message {
            role: Role::Assistant,
            content: String::new(),
            tool_call_id: None,
            tool_calls: vec![ToolCall {
                id: "1".into(),
                name: "shell".into(),
                arguments: "{\"command\":\"ls\"}".into(),
            }],
        });
        log.log_command("ls", 0, 5, "file listing\n");
        log.write_final("done");
        let id = log.id().to_string();
        drop(log);

        // list shows the session.
        list(tmp.path()).unwrap();
        // replay does not error.
        replay(tmp.path(), &id).unwrap();
        // unknown id errors.
        assert!(replay(tmp.path(), "nope").is_err());
    }
}
