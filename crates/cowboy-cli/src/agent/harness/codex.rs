//! OpenAI Codex (`codex exec --json`) → cowboy UI events.
//!
//! One JSON object per line, keyed by `type`:
//!
//! - `thread.started` (`thread_id`), `turn.started`.
//! - `item.started` / `item.updated` / `item.completed` with an `item` whose `type` is
//!   `agent_message` (`text`), `reasoning` (`text`), `command_execution` (`command`,
//!   `aggregated_output`, `exit_code`, `status`), `file_change` (`changes[]` of
//!   `path` + `kind`), `mcp_tool_call` (`server`, `tool`) or `web_search` (`query`).
//! - `turn.completed` (`usage`: `input_tokens`, `cached_input_tokens`,
//!   `output_tokens`), `turn.failed` (`error.message`), `error` (`message`).
//!
//! The failure path was recorded live from 0.154.0
//! (`testdata/codex-0.154.0-usage-limit.jsonl`: a plan's usage limit). The success
//! path follows the same schema and is covered by a constructed stream until a live
//! success can be recorded. `codex exec -o <file>` writes the final message too; the
//! driver prefers that file when it exists.

use serde_json::Value;

use super::StreamParser;
use crate::agent::ui::AgentUi;

#[derive(Debug, Default)]
pub struct CodexStream {
    last_message: String,
    tokens: (u64, u64),
    error: Option<String>,
    saw_events: bool,
}

impl StreamParser for CodexStream {
    fn on_line(&mut self, line: &str, ui: &mut dyn AgentUi) {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            return;
        };
        let Some(kind) = v.get("type").and_then(Value::as_str) else {
            return;
        };
        self.saw_events = true;
        match kind {
            "item.started" | "item.completed" => {
                let item = v.get("item").cloned().unwrap_or(Value::Null);
                let done = kind == "item.completed";
                match item.get("type").and_then(Value::as_str) {
                    Some("agent_message") if done => {
                        let t = str_of(&item, "text");
                        ui.model_delta(&t);
                        ui.model_done();
                        self.last_message = t;
                    }
                    Some("reasoning") if done => ui.model_reasoning(&str_of(&item, "text")),
                    Some("command_execution") => {
                        if done {
                            let out = str_of(&item, "aggregated_output");
                            if !out.is_empty() {
                                ui.command_output(&out);
                            }
                            let code = item
                                .get("exit_code")
                                .and_then(Value::as_i64)
                                .map(|c| c as i32)
                                .unwrap_or(
                                    if item.get("status").and_then(Value::as_str) == Some("failed")
                                    {
                                        1
                                    } else {
                                        0
                                    },
                                );
                            ui.command_end(code, "");
                        } else {
                            ui.command_start(&str_of(&item, "command"));
                        }
                    }
                    Some("file_change") if done => {
                        for c in item
                            .get("changes")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                        {
                            ui.tool_use(&format!(
                                "{} {}",
                                c.get("kind").and_then(Value::as_str).unwrap_or("edit"),
                                str_of(c, "path")
                            ));
                        }
                    }
                    Some("mcp_tool_call") if !done => ui.tool_use(&format!(
                        "{}.{}",
                        str_of(&item, "server"),
                        str_of(&item, "tool")
                    )),
                    Some("web_search") if !done => {
                        ui.tool_use(&format!("web_search {}", str_of(&item, "query")))
                    }
                    _ => {}
                }
            }
            "turn.completed" => {
                if let Some(u) = v.get("usage") {
                    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                    self.tokens.0 += n("input_tokens") + n("cached_input_tokens");
                    self.tokens.1 += n("output_tokens");
                    ui.tokens(self.tokens.0, self.tokens.1);
                }
            }
            "turn.failed" => {
                self.error = v
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| self.error.take());
            }
            "error" => {
                if let Some(m) = v.get("message").and_then(Value::as_str) {
                    self.error = Some(m.to_string());
                }
            }
            _ => {}
        }
    }

    fn saw_events(&self) -> bool {
        self.saw_events
    }

    fn answer(&self) -> String {
        self.last_message.trim().to_string()
    }

    fn error(&self) -> Option<String> {
        self.error.clone()
    }
}

fn str_of(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::super::tests::Rec;
    use super::*;

    /// Recorded live: a ChatGPT plan out of quota. The job must fail with that
    /// message, not report an empty success.
    #[test]
    fn a_recorded_usage_limit_is_an_error() {
        let mut s = CodexStream::default();
        let mut ui = Rec::default();
        for line in include_str!("testdata/codex-0.154.0-usage-limit.jsonl").lines() {
            s.on_line(line, &mut ui);
        }
        assert!(s.saw_events());
        assert!(
            s.error().unwrap().contains("usage limit"),
            "{:?}",
            s.error()
        );
    }

    #[test]
    fn a_successful_run_maps_to_ui_events_and_an_answer() {
        let stream = [
            r#"{"type":"thread.started","thread_id":"t1"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.completed","item":{"id":"i0","type":"reasoning","text":"list first"}}"#,
            r#"{"type":"item.started","item":{"id":"i1","type":"command_execution","command":"bash -lc ls","aggregated_output":"","status":"in_progress"}}"#,
            r#"{"type":"item.completed","item":{"id":"i1","type":"command_execution","command":"bash -lc ls","aggregated_output":"a.txt\n","exit_code":0,"status":"completed"}}"#,
            r#"{"type":"item.completed","item":{"id":"i2","type":"file_change","changes":[{"path":"b.txt","kind":"add"}],"status":"completed"}}"#,
            r#"{"type":"item.completed","item":{"id":"i3","type":"agent_message","text":"DONE"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":900,"cached_input_tokens":100,"output_tokens":20}}"#,
        ];
        let mut s = CodexStream::default();
        let mut ui = Rec::default();
        for l in stream {
            s.on_line(l, &mut ui);
        }
        assert_eq!(ui.commands, vec![("bash -lc ls".to_string(), 0)]);
        assert!(ui.tools.iter().any(|t| t == "add b.txt"), "{:?}", ui.tools);
        assert_eq!(s.answer(), "DONE");
        assert_eq!(ui.tokens, Some((1000, 20)));
        assert!(s.error().is_none());
    }
}
