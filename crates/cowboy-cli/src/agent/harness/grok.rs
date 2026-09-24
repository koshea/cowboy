//! Grok Build (`grok` 1.x) headless stream → cowboy UI events.
//!
//! `grok --output-format streaming-json` prints one JSON object per line (ACP session
//! updates). The shapes below were recorded from a live 1.0.41 run
//! (`testdata/grok-1.0.41.jsonl`); an unknown `type` is ignored, never fatal, since
//! the CLI adds event types between releases.
//!
//! - `thought` / `text` — reasoning / answer deltas (`data`).
//! - `tool_call` — a tool starting: `toolName`, `kind`, `rawInput` (a terminal
//!   command's `command`, an edit's `file_path`, …).
//! - `tool_call_update` — progress; `status: "completed"` carries `rawOutput`
//!   (`exit_code`, `output_for_prompt` for a terminal command) or `diff` content.
//! - `end` — `stopReason`, `sessionId`, `num_turns`, `usage`, `total_cost_usd`.
//!
//! Grok has no single "final answer" event: the answer is the text after the last
//! tool call (earlier text is narration between tool calls).

use std::collections::HashMap;

use serde_json::Value;

use super::StreamParser;
use crate::agent::ui::AgentUi;

/// Accumulated state of one grok run.
#[derive(Debug, Default)]
pub struct GrokStream {
    /// Text since the last tool call — the answer, once the run ends.
    segment: String,
    /// Every text delta, for a run that ends mid-tool (no clean answer segment).
    all_text: String,
    /// Tool calls in flight: id → the command it runs (terminal) or `None`.
    open: HashMap<String, Option<String>>,
    pub session_id: Option<String>,
    pub stop_reason: Option<String>,
    pub turns: Option<u64>,
    pub cost_usd: Option<f64>,
    pub tokens: Option<(u64, u64)>,
    /// Whether any line parsed at all — distinguishes "grok printed nothing we
    /// understand" (wrong flags, a version change) from an empty answer.
    pub saw_events: bool,
}

impl StreamParser for GrokStream {
    fn on_line(&mut self, line: &str, ui: &mut dyn AgentUi) {
        self.handle(line, ui);
    }
    fn saw_events(&self) -> bool {
        self.saw_events
    }
    fn answer(&self) -> String {
        self.final_answer()
    }
    fn error(&self) -> Option<String> {
        None
    }
}

impl GrokStream {
    /// Handle one line of the stream.
    fn handle(&mut self, line: &str, ui: &mut dyn AgentUi) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let Some(kind) = v.get("type").and_then(Value::as_str) else {
            return;
        };
        self.saw_events = true;
        match kind {
            "thought" => {
                if let Some(d) = v.get("data").and_then(Value::as_str) {
                    ui.model_reasoning(d);
                }
            }
            "text" => {
                if let Some(d) = v.get("data").and_then(Value::as_str) {
                    ui.model_delta(d);
                    self.segment.push_str(d);
                    self.all_text.push_str(d);
                }
            }
            "tool_call" => self.tool_call(&v, ui),
            "tool_call_update" => self.tool_update(&v, ui),
            "end" => {
                self.session_id = str_field(&v, "sessionId");
                self.stop_reason = str_field(&v, "stopReason");
                self.turns = v.get("num_turns").and_then(Value::as_u64);
                if let Some(u) = v.get("usage") {
                    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                    let input = n("input_tokens") + n("cache_read_input_tokens");
                    let tokens = (input, n("output_tokens"));
                    ui.tokens(tokens.0, tokens.1);
                    self.tokens = Some(tokens);
                }
                if let Some(c) = v.get("total_cost_usd").and_then(Value::as_f64) {
                    ui.cost(c);
                    self.cost_usd = Some(c);
                }
            }
            _ => {}
        }
    }

    fn tool_call(&mut self, v: &Value, ui: &mut dyn AgentUi) {
        // Text so far was narration before a tool, not the answer.
        ui.model_done();
        self.segment.clear();
        let id = str_field(v, "toolCallId").unwrap_or_default();
        let name = str_field(v, "toolName")
            .or_else(|| str_field(v, "title"))
            .unwrap_or_else(|| "tool".into());
        let input = v.get("rawInput").cloned().unwrap_or(Value::Null);
        let command = (str_field(v, "kind").as_deref() == Some("execute"))
            .then(|| {
                input
                    .get("command")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .flatten();
        match &command {
            Some(cmd) => ui.command_start(cmd),
            None => ui.tool_use(&summarize(&name, &input)),
        }
        self.open.insert(id, command);
    }

    fn tool_update(&mut self, v: &Value, ui: &mut dyn AgentUi) {
        let status = str_field(v, "status");
        if !matches!(status.as_deref(), Some("completed") | Some("failed")) {
            return;
        }
        let id = str_field(v, "toolCallId").unwrap_or_default();
        let Some(command) = self.open.remove(&id) else {
            return;
        };
        let raw = v.get("rawOutput").cloned().unwrap_or(Value::Null);
        if command.is_some() {
            let output = raw
                .get("output_for_prompt")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !output.is_empty() {
                ui.command_output(output);
            }
            let code = raw
                .get("exit_code")
                .and_then(Value::as_i64)
                .map(|c| c as i32)
                .unwrap_or(if status.as_deref() == Some("failed") {
                    1
                } else {
                    0
                });
            ui.command_end(code, "");
        }
        // Edits carry their diff as content blocks.
        for block in v
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if block.get("type").and_then(Value::as_str) == Some("diff") {
                let path = str_field(block, "path").unwrap_or_default();
                let old = str_field(block, "oldText").unwrap_or_default();
                let new = str_field(block, "newText").unwrap_or_default();
                ui.file_diff(&path, &simple_diff(&path, &old, &new));
            }
        }
    }

    /// The run's answer: the text after the last tool call, else all of it.
    fn final_answer(&self) -> String {
        let seg = self.segment.trim();
        if seg.is_empty() {
            self.all_text.trim().to_string()
        } else {
            seg.to_string()
        }
    }
}

fn str_field(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

/// A one-line summary of a non-terminal tool call, for the transcript.
fn summarize(name: &str, input: &Value) -> String {
    let arg = ["file_path", "path", "pattern", "query", "url"]
        .iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str));
    match arg {
        Some(a) => format!("{name} {a}"),
        None => name.to_string(),
    }
}

/// A minimal unified diff of a whole-text change — enough for the transcript's diff
/// view; the host-measured `git diff --stat` in the result is the authority.
fn simple_diff(path: &str, old: &str, new: &str) -> String {
    let rel = path.trim_start_matches('/');
    let mut out = format!("--- a/{rel}\n+++ b/{rel}\n");
    for l in old.lines() {
        out.push_str(&format!("-{l}\n"));
    }
    for l in new.lines() {
        out.push_str(&format!("+{l}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::tests::Rec;
    use super::*;

    /// The recorded 1.0.41 run: `ls`, create b.txt, reply "DONE".
    #[test]
    fn a_recorded_run_maps_to_ui_events_and_an_answer() {
        let mut s = GrokStream::default();
        let mut ui = Rec::default();
        for line in include_str!("testdata/grok-1.0.41.jsonl").lines() {
            s.on_line(line, &mut ui);
        }
        assert!(s.saw_events);
        assert_eq!(ui.commands, vec![("ls".to_string(), 0)]);
        assert!(
            ui.tools.iter().any(|t| t.contains("search_replace")),
            "{:?}",
            ui.tools
        );
        assert_eq!(ui.diffs, vec!["/workspace/b.txt".to_string()]);
        assert_eq!(
            s.answer(),
            "DONE",
            "the answer is the text after the last tool"
        );
        assert_eq!(s.stop_reason.as_deref(), Some("end_turn"));
        assert!(s.session_id.is_some());
        assert_eq!(s.turns, Some(3));
        assert!(ui.cost.unwrap() > 0.0);
        assert!(ui.tokens.unwrap().1 > 0);
    }

    #[test]
    fn junk_and_unknown_events_are_ignored() {
        let mut s = GrokStream::default();
        let mut ui = Rec::default();
        for l in [
            "",
            "not json",
            "{\"no\":\"type\"}",
            "{\"type\":\"future_thing\"}",
        ] {
            s.on_line(l, &mut ui);
        }
        assert!(
            s.saw_events,
            "a known shape with an unknown type still counts"
        );
        assert_eq!(s.answer(), "");
    }
}
