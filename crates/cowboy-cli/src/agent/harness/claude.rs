//! Claude Code (`claude -p --output-format stream-json --verbose`) → cowboy UI events.
//!
//! Recorded from a live 2.1.281 run (`testdata/claude-2.1.281.jsonl`). One JSON object
//! per line, keyed by `type`:
//!
//! - `system` (`subtype: init`) — session start.
//! - `assistant` — a model message; `message.content[]` holds `text`, `thinking` and
//!   `tool_use` (`id`, `name`, `input`) blocks.
//! - `user` — tool results: `message.content[]` `tool_result` blocks (`tool_use_id`,
//!   `content`, `is_error`).
//! - `result` — the end: `result` (the final answer), `is_error`, `num_turns`,
//!   `total_cost_usd`, `usage`, `session_id`.
//!
//! Messages from Claude's own subagents carry a `parent_tool_use_id` and are skipped:
//! the transcript shows the top-level run, as it does for cowboy's own loop.

use std::collections::HashMap;

use serde_json::Value;

use super::StreamParser;
use crate::agent::ui::AgentUi;

#[derive(Debug, Default)]
pub struct ClaudeStream {
    segment: String,
    /// tool_use id → whether it is a shell command (Bash).
    open: HashMap<String, bool>,
    result: Option<String>,
    error: Option<String>,
    saw_events: bool,
}

impl StreamParser for ClaudeStream {
    fn on_line(&mut self, line: &str, ui: &mut dyn AgentUi) {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            return;
        };
        let Some(kind) = v.get("type").and_then(Value::as_str) else {
            return;
        };
        self.saw_events = true;
        if v.get("parent_tool_use_id").is_some_and(|p| !p.is_null()) {
            return;
        }
        match kind {
            "assistant" => {
                for block in blocks(&v) {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let t = str_of(block, "text");
                            ui.model_delta(&t);
                            self.segment.push_str(&t);
                        }
                        Some("thinking") => ui.model_reasoning(&str_of(block, "thinking")),
                        Some("tool_use") => {
                            ui.model_done();
                            self.segment.clear();
                            let id = str_of(block, "id");
                            let name = str_of(block, "name");
                            let input = block.get("input").cloned().unwrap_or(Value::Null);
                            let is_shell = name == "Bash";
                            if is_shell {
                                ui.command_start(&str_of(&input, "command"));
                            } else {
                                ui.tool_use(&summarize(&name, &input));
                            }
                            self.open.insert(id, is_shell);
                        }
                        _ => {}
                    }
                }
            }
            "user" => {
                for block in blocks(&v) {
                    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let id = str_of(block, "tool_use_id");
                    if self.open.remove(&id) == Some(true) {
                        let out = result_text(block.get("content"));
                        if !out.is_empty() {
                            ui.command_output(&out);
                        }
                        let failed = block.get("is_error").and_then(Value::as_bool) == Some(true);
                        ui.command_end(i32::from(failed), "");
                    }
                }
            }
            "result" => {
                if let Some(u) = v.get("usage") {
                    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                    let input = n("input_tokens")
                        + n("cache_read_input_tokens")
                        + n("cache_creation_input_tokens");
                    ui.tokens(input, n("output_tokens"));
                }
                if let Some(c) = v.get("total_cost_usd").and_then(Value::as_f64) {
                    ui.cost(c);
                }
                let text = v.get("result").and_then(Value::as_str).map(str::to_string);
                if v.get("is_error").and_then(Value::as_bool) == Some(true) {
                    self.error = Some(text.clone().unwrap_or_else(|| str_of(&v, "subtype")));
                }
                self.result = text;
            }
            _ => {}
        }
    }

    fn saw_events(&self) -> bool {
        self.saw_events
    }

    fn answer(&self) -> String {
        self.result
            .clone()
            .unwrap_or_else(|| self.segment.clone())
            .trim()
            .to_string()
    }

    fn error(&self) -> Option<String> {
        self.error.clone()
    }
}

fn blocks(v: &Value) -> impl Iterator<Item = &Value> {
    v.pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn str_of(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// A tool result's text: a plain string, or the text blocks of a content array.
fn result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn summarize(name: &str, input: &Value) -> String {
    let arg = ["file_path", "path", "pattern", "url", "query"]
        .iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str));
    match arg {
        Some(a) => format!("{name} {a}"),
        None => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::Rec;
    use super::*;

    #[test]
    fn a_recorded_run_maps_to_ui_events_and_an_answer() {
        let mut s = ClaudeStream::default();
        let mut ui = Rec::default();
        for line in include_str!("testdata/claude-2.1.281.jsonl").lines() {
            s.on_line(line, &mut ui);
        }
        assert!(s.saw_events());
        assert_eq!(ui.commands.len(), 1);
        assert!(ui.commands[0].0.contains("b.txt"), "{:?}", ui.commands);
        assert_eq!(ui.commands[0].1, 0);
        assert_eq!(s.answer(), "DONE");
        assert!(s.error().is_none());
        assert!(ui.cost.unwrap() > 0.0);
        assert!(ui.tokens.unwrap().0 > 0);
    }

    #[test]
    fn an_error_result_is_reported() {
        let mut s = ClaudeStream::default();
        let mut ui = Rec::default();
        s.on_line(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"Claude AI usage limit reached"}"#,
            &mut ui,
        );
        assert_eq!(s.error().as_deref(), Some("Claude AI usage limit reached"));
    }
}
