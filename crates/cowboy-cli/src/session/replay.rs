//! Listing and read-only replay of past sessions.

use std::path::Path;

use anyhow::{bail, Context, Result};
use cowboy_core::model::{Message, Role};

use super::{session_dir, sessions_dir, CommandRecord};

/// The small amount of reliable metadata available without the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskSession {
    pub id: String,
    pub started_ms: u64,
    pub activity: String,
    pub attention: String,
    pub summary: String,
}

/// Direct session-directory names, newest first. Symlinks are not sessions.
pub fn ids(root: &Path) -> Result<Vec<String>> {
    let dir = sessions_dir(root);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut ids: Vec<String> = std::fs::read_dir(&dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    ids.sort_by(|a, b| {
        session_started_ms(b)
            .cmp(&session_started_ms(a))
            .then(b.cmp(a))
    });
    Ok(ids)
}

/// Resolve an exact or unambiguous prefix against known session ids.
///
/// Resolution happens against enumerated names rather than by joining unchecked
/// input onto the sessions directory, so a short id cannot become a path escape.
pub fn resolve_from<'a>(
    input: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Result<String> {
    if input.is_empty() {
        bail!("session id cannot be empty");
    }
    let mut matches: Vec<&str> = candidates
        .into_iter()
        .filter(|candidate| candidate.starts_with(input))
        .collect();
    matches.sort_unstable();
    matches.dedup();
    if matches.binary_search(&input).is_ok() {
        return Ok(input.to_string());
    }
    match matches.as_slice() {
        [] => bail!("no such session: {input}"),
        [id] => Ok((*id).to_string()),
        _ => bail!(
            "ambiguous session id {input:?}; matches: {}",
            matches.join(", ")
        ),
    }
}

/// Resolve an exact or unambiguous current-project on-disk session id.
pub fn resolve_id(root: &Path, input: &str) -> Result<String> {
    let candidates = ids(root)?;
    resolve_from(input, candidates.iter().map(String::as_str))
}

/// Read current-project disk history for the merged session browser.
pub fn disk_sessions(root: &Path) -> Result<Vec<DiskSession>> {
    ids(root)?
        .into_iter()
        .map(|id| {
            let dir = session_dir(root, &id);
            let messages = line_count(&dir.join("transcript.jsonl"));
            let commands = line_count(&dir.join("commands.jsonl"));
            let activity = match (messages, commands) {
                (0, 0) => "saved".to_string(),
                (m, 0) => format!("{m} msg"),
                (0, c) => format!("{c} cmd"),
                (m, c) => format!("{m} msg · {c} cmd"),
            };
            let final_path = dir.join("final.md");
            let summary = std::fs::read_to_string(&final_path)
                .ok()
                .and_then(|text| text.lines().next().map(str::to_string))
                .filter(|line| !line.trim().is_empty())
                .unwrap_or_else(|| "(no final summary)".into());
            let has_final = summary != "(no final summary)";
            let attention = if has_final {
                "-".to_string()
            } else {
                "incomplete".to_string()
            };
            Ok(DiskSession {
                started_ms: session_started_ms(&id),
                id,
                activity,
                attention,
                summary,
            })
        })
        .collect()
}

/// List sessions newest-first with a one-line summary.
pub fn list(root: &Path) -> Result<()> {
    let sessions = disk_sessions(root)?;
    if sessions.is_empty() {
        println!("no sessions yet");
        return Ok(());
    }
    for session in sessions {
        println!("{}  {}", session.id, session.summary);
    }
    Ok(())
}

/// Render a past session to stdout.
pub fn replay(root: &Path, id: &str) -> Result<()> {
    let id = resolve_id(root, id)?;
    let dir = session_dir(root, &id);

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
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        if !recs.is_empty() {
            println!("\n--- commands ({}) ---", recs.len());
            for record in recs {
                println!(
                    "  [{}] exit={} {}ms  {}",
                    record.seq, record.exit_code, record.duration_ms, record.command
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
            let count = text.lines().filter(|line| !line.trim().is_empty()).count();
            if count > 0 {
                println!("--- {label} ({count}) ---");
                for line in text.lines().take(20) {
                    println!("  {line}");
                }
            }
        }
    }
    let artifacts = cowboy_core::artifact::list_in(&dir);
    if !artifacts.is_empty() {
        println!("--- artifacts ({}) ---", artifacts.len());
        for artifact in &artifacts {
            println!(
                "  {} [{}] {}",
                artifact.id,
                artifact.kind.as_str(),
                artifact.title
            );
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

fn session_started_ms(id: &str) -> u64 {
    id.split_once('-')
        .map_or(id, |(timestamp, _)| timestamp)
        .parse()
        .unwrap_or(0)
}

fn line_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0)
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
    fn resolves_exact_and_unambiguous_short_ids() {
        let ids = ["1789651284235-1", "1789651284235-22", "1789659999999-3"];
        assert_eq!(resolve_from("178965999", ids).unwrap(), "1789659999999-3");
        assert_eq!(
            resolve_from("1789651284235-1", ids).unwrap(),
            "1789651284235-1",
            "an exact id wins even when it prefixes another id"
        );
        let error = resolve_from("178965128", ids).unwrap_err();
        assert!(error.to_string().contains("ambiguous"), "{error:#}");
        assert!(resolve_from("../outside", ids).is_err());
        assert!(resolve_from("", ids).is_err());
    }

    #[test]
    fn disk_history_is_newest_first_and_reports_attention() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let old = SessionLogger::create_with_id(tmp.path(), "100-1").unwrap();
        old.write_final("done");
        drop(old);
        let mut newest = SessionLogger::create_with_id(tmp.path(), "200-1").unwrap();
        newest.log_message(&Message::user("still working"));
        drop(newest);

        let rows = disk_sessions(tmp.path()).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["200-1", "100-1"]
        );
        assert_eq!(rows[0].activity, "1 msg");
        assert_eq!(rows[0].attention, "incomplete");
        assert_eq!(rows[1].attention, "-");
    }

    #[test]
    fn replay_renders_a_recorded_session_and_accepts_a_short_id() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let mut log = SessionLogger::create_with_id(tmp.path(), "1789651284235-123").unwrap();
        log.log_message(&Message::user("inspect repo"));
        log.log_message(&Message {
            reasoning: None,
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
        drop(log);

        list(tmp.path()).unwrap();
        replay(tmp.path(), "178965128").unwrap();
        assert!(replay(tmp.path(), "nope").is_err());
    }
}
