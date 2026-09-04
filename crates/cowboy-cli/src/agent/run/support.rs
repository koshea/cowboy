//! Pure helpers for the agent loop: no `AgentLoop` state, just data in → data
//! out (rendering, parsing, diffing, truncation). Split out of `run/mod.rs` to
//! keep the loop itself focused on orchestration.

use super::*;
use std::path::PathBuf;

/// Path to *this* cowboy binary, for spawning subagents.
///
/// Delegates to [`crate::project::self_exe`], which is robust to the binary being
/// replaced mid-session. Shared with the sandbox's shim bind, which hits the same
/// problem far less visibly — see that function.
pub(super) fn self_exe() -> std::result::Result<PathBuf, String> {
    crate::project::self_exe()
}

/// Forward a streamed [`Delta`] to the UI. A free function so it borrows only
/// the UI, not all of `self` (the in-flight chat future holds an immutable
/// borrow of the loop).
pub(super) fn emit_delta(ui: &mut dyn AgentUi, piece: Delta) {
    match piece {
        Delta::Content(t) => ui.model_delta(&t),
        Delta::Reasoning(t) => ui.model_reasoning(&t),
    }
}

/// Render a span of messages as plain text for the compaction summarizer.
pub(super) fn render_transcript(messages: &[Message]) -> String {
    let mut s = String::new();
    for m in messages {
        let role = match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        s.push_str(&format!("[{role}]\n"));
        if !m.content.is_empty() {
            s.push_str(&m.content);
            s.push('\n');
        }
        for tc in &m.tool_calls {
            s.push_str(&format!("(tool call {}: {})\n", tc.name, tc.arguments));
        }
        s.push('\n');
    }
    s
}

pub(super) fn parse_args<T: serde::de::DeserializeOwned>(arguments: &str) -> Result<T> {
    let args = if arguments.trim().is_empty() {
        "{}"
    } else {
        arguments
    };
    serde_json::from_str(args).map_err(|e| anyhow::anyhow!("invalid tool arguments: {e}"))
}

/// Render a [`HandoffArgs`] into the canonical `handoff.md` markdown.
pub(super) fn render_handoff_md(a: &HandoffArgs) -> String {
    let mut s = String::from("# Handoff\n\n");
    s.push_str(&format!("## Goal\n{}\n\n", a.goal.trim()));
    s.push_str(&format!("## Status\n{}\n", a.status.trim()));
    let section = |title: &str, body: &Option<String>| -> String {
        match body {
            Some(b) if !b.trim().is_empty() => format!("\n## {title}\n{}\n", b.trim()),
            _ => String::new(),
        }
    };
    s.push_str(&section("Changed files", &a.changed_files));
    s.push_str(&section("Decisions", &a.decisions));
    s.push_str(&section("Contracts / interfaces", &a.contracts));
    s.push_str(&section("Validation", &a.validation));
    s.push_str(&section("Risks", &a.risks));
    s.push_str(&section("Next steps", &a.next_steps));
    s
}

/// Render a plan as check-boxed lines (for the model observation / console).
pub(super) fn render_plan(plan: &[(String, String)]) -> String {
    plan.iter()
        .map(|(step, status)| {
            let mark = match status.as_str() {
                "done" => "[x]",
                "in_progress" => "[~]",
                _ => "[ ]",
            };
            format!("{mark} {step}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A concise one-line summary of a file op for the UI: the helper's status line
/// on success, or `"<action> <path> — failed"` otherwise.
pub(super) fn fileop_summary(action: &str, path: &str, exit: i32, output: &str) -> String {
    if exit == 0 {
        let line = output.trim();
        if line.is_empty() {
            format!("{action} {path}")
        } else {
            line.to_string()
        }
    } else {
        format!("{action} {path} — failed")
    }
}

/// Build a unified diff (`--- a/path` / `+++ b/path` headers + hunks) of a file
/// change, capped at `max_lines` rendered lines (a trailing marker notes the
/// elision). Returns empty for an unchanged or binary-looking file.
pub(super) fn unified_diff(path: &str, before: &str, after: &str, max_lines: usize) -> String {
    // Skip likely-binary content (NUL bytes) — a diff would be noise.
    if before.contains('\u{0}') || after.contains('\u{0}') {
        return String::new();
    }
    let diff = similar::TextDiff::from_lines(before, after);
    let body = diff
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();
    if body.trim().is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = body.lines().collect();
    if lines.len() > max_lines {
        let kept = lines[..max_lines].join("\n");
        let hidden = lines.len() - max_lines;
        format!("{kept}\n… {hidden} more diff lines (see the file)")
    } else {
        body
    }
}

/// A stable signature for a turn's tool calls (name + arguments), order-
/// independent so parallel calls in a different order still compare equal. Used
/// by the loop guard to detect an agent re-issuing the identical action.
pub(super) fn tool_signature(calls: &[cowboy_core::model::ToolCall]) -> String {
    let mut parts: Vec<String> = calls
        .iter()
        .map(|c| format!("{}\u{0}{}", c.name, c.arguments))
        .collect();
    parts.sort();
    parts.join("\u{1}")
}

/// Truncate `output` to at most `max_bytes`, on a char boundary, with a marker.
pub(super) fn truncate(output: &str, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !output.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[... output truncated at {} bytes ...]",
        &output[..end],
        max_bytes
    )
}
