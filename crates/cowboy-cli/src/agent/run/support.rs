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
///
/// `shell` arguments are normalized ([`normalize_shell_args`]) so a model that
/// re-runs the *same inspection* with cosmetic churn — appending `| wc -l`, a
/// trailing `; echo "..."` probe, a `2>&1`, or just fiddling whitespace — still
/// collapses to one signature. Without this the guard only caught byte-identical
/// repetition, and a fixating model would tweak the tail every turn and burn all
/// `max_iterations` (observed: 17 turns re-running one `git show | awk | grep`
/// pipeline with a changing tail, each producing a slightly different count).
pub(super) fn tool_signature(calls: &[cowboy_core::model::ToolCall]) -> String {
    signature(calls, true)
}

/// The *raw* signature: name + arguments with no normalization. Byte-identical
/// calls compare equal; a cosmetic edit does not. The loop guard uses the
/// difference between this and [`tool_signature`] to tell exact repetition
/// (polling) from cosmetic churn.
pub(super) fn raw_tool_signature(calls: &[cowboy_core::model::ToolCall]) -> String {
    signature(calls, false)
}

/// Shared body for the two signatures. `normalize` folds cosmetic churn in
/// `shell` commands; without it the arguments are compared verbatim.
fn signature(calls: &[cowboy_core::model::ToolCall], normalize: bool) -> String {
    let mut parts: Vec<String> = calls
        .iter()
        .map(|c| {
            let args = if normalize && c.name == "shell" {
                normalize_shell_args(&c.arguments)
            } else {
                c.arguments.clone()
            };
            format!("{}\u{0}{}", c.name, args)
        })
        .collect();
    parts.sort();
    parts.join("\u{1}")
}

/// Normalize a `shell` tool's JSON arguments for loop-guard comparison: parse out
/// the `command`, strip cosmetic tails that don't change *what is being
/// inspected*, and re-emit. Falls back to the raw string if the JSON or the
/// `command` field isn't shaped as expected — a normalization miss only makes the
/// guard slightly less sensitive, never wrong.
///
/// Deliberately conservative: it strips only additive noise (trailing counters,
/// `echo` narration, stderr-merge, whitespace), never rewrites the substantive
/// pipeline. Over-normalizing would collapse *legitimate* iterative refinement
/// into a false loop and abort real work.
fn normalize_shell_args(arguments: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return arguments.to_string();
    };
    let Some(cmd) = v.get("command").and_then(|c| c.as_str()) else {
        return arguments.to_string();
    };
    format!("command={}", normalize_shell_command(cmd))
}

/// The cosmetic-churn stripper. Splits the command on the segment separators a
/// model uses to append probes (`;`, `&&`, `|`), drops segments that are pure
/// narration or counting (`echo …`, `wc -l/-c`, `true`), strips a trailing
/// `2>&1`, collapses whitespace, and rejoins. What remains is the substantive
/// work; if that is unchanged across turns the model is not making progress.
fn normalize_shell_command(cmd: &str) -> String {
    // Drop shell line-continuations (`\` + newline) so a reflowed command doesn't
    // leave a stray backslash token, then collapse whitespace runs (incl.
    // newlines) to single spaces so reflowed-but-identical commands compare equal.
    let joined = cmd.replace("\\\n", " ");
    let flat = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    // Strip a trailing stderr-merge, a common no-op tweak.
    let flat = flat.strip_suffix(" 2>&1").unwrap_or(&flat).trim();

    // Split into segments on `;`, `&&`, `||`, and `|` so we can drop the
    // cosmetic ones. This is a coarse split (it ignores quoting), which is fine:
    // the result feeds a similarity hash, not an executor.
    let segments = flat
        .split(&[';', '|'][..])
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let kept: Vec<String> = segments
        .filter(|seg| !is_cosmetic_segment(seg))
        .map(|seg| {
            // `grep -c PAT` (count) and `grep PAT` (print) inspect the same thing;
            // fold the count flag away so switching between them isn't "progress".
            seg.replace("grep -c ", "grep ")
                .replace("grep -cE ", "grep -E ")
                .replace("grep -ci ", "grep -i ")
        })
        .collect();

    kept.join("|")
}

/// A command segment that only reports/echoes and doesn't change what is being
/// inspected: `echo …`, `wc -l/-c/-m`, a bare `wc`, `head`/`tail` line-count
/// tweaks, and shell no-ops. These are exactly the tails a stuck model appends
/// turn over turn.
fn is_cosmetic_segment(seg: &str) -> bool {
    let head = seg.split_whitespace().next().unwrap_or("");
    matches!(head, "echo" | "printf" | "true" | ":") || seg == "wc" || seg.starts_with("wc -")
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

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::model::ToolCall;

    fn shell(cmd: &str) -> Vec<ToolCall> {
        vec![ToolCall {
            id: "x".into(),
            name: "shell".into(),
            arguments: serde_json::json!({ "command": cmd }).to_string(),
        }]
    }

    #[test]
    fn whitespace_and_stderr_merge_are_cosmetic() {
        let a = tool_signature(&shell("git show HEAD | grep -E foo"));
        let b = tool_signature(&shell("git  show   HEAD  |  grep -E foo 2>&1"));
        let c = tool_signature(&shell("git show HEAD \\\n | grep -E foo"));
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn appended_echo_and_wc_probes_are_cosmetic() {
        let base = tool_signature(&shell("git show HEAD | grep -E foo"));
        // A trailing counter …
        assert_eq!(
            base,
            tool_signature(&shell("git show HEAD | grep -E foo | wc -l"))
        );
        // … an appended echo narration …
        assert_eq!(
            base,
            tool_signature(&shell("git show HEAD | grep -E foo; echo \"done\""))
        );
        // … and both at once.
        assert_eq!(
            base,
            tool_signature(&shell(
                "git show HEAD | grep -E foo | wc -l; echo \"exit=$?\""
            ))
        );
    }

    #[test]
    fn grep_count_flag_folds_to_the_same_inspection() {
        let print = tool_signature(&shell("git show HEAD | grep -E foo"));
        let count = tool_signature(&shell("git show HEAD | grep -cE foo"));
        assert_eq!(print, count);
    }

    #[test]
    fn a_genuinely_different_command_keeps_a_distinct_signature() {
        // Changing the *substance* (the file, the pattern) must NOT collapse —
        // that would abort legitimate iterative refinement.
        let a = tool_signature(&shell("git show HEAD -- ranch.rs | grep -E foo"));
        let b = tool_signature(&shell("git show HEAD -- scope.rs | grep -E foo"));
        let c = tool_signature(&shell("git show HEAD -- ranch.rs | grep -E bar"));
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn non_shell_arguments_are_left_verbatim() {
        // Only `shell` is normalized; other tools compare on raw arguments.
        let a = vec![ToolCall {
            id: "1".into(),
            name: "read".into(),
            arguments: r#"{"path":"/a  b"}"#.into(),
        }];
        let b = vec![ToolCall {
            id: "2".into(),
            name: "read".into(),
            arguments: r#"{"path":"/a b"}"#.into(),
        }];
        assert_ne!(tool_signature(&a), tool_signature(&b));
    }

    #[test]
    fn the_observed_churn_burst_collapses_to_one_signature() {
        // Verbatim tails from session 1788790179664-2 turns 85–100: one
        // `git show … | awk … | grep …` pipeline the model kept re-issuing with a
        // changing cosmetic tail. Pre-fix each had a distinct signature and slipped
        // the guard; post-fix they must all collapse so the guard fires.
        let core = "git show 21c9a35 --format=\"\" -- crates/cowboy-cli/src/cmd/ranch.rs \
                    | awk '/^@@ -183/{p=1} /^@@ -376/{exit} p' | grep -E \"^[+-]\" \
                    | grep -vE \"^[+-][+-]\" | grep -E \"test|dead-sid\"";
        let variants = [
            format!("cd /workspace && {core} | wc -l"),
            format!("cd /workspace && {core} | wc -l; echo \"exit=$?\""),
            format!("cd /workspace && {core} | wc -l 2>&1; echo done"),
            format!("cd /workspace && {core}   |   wc -l"),
        ];
        let first = tool_signature(&shell(&variants[0]));
        for v in &variants[1..] {
            assert_eq!(
                first,
                tool_signature(&shell(v)),
                "variant should collapse: {v}"
            );
        }
    }
}
