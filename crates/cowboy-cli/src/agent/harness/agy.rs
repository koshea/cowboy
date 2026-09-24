//! Google Antigravity (`agy -p --output-format stream-json`) → cowboy UI events.
//!
//! Recorded from a live 1.2.9 run (`testdata/agy-1.2.9.jsonl`). One JSON object per
//! line, keyed by `event` (not `type`):
//!
//! - `init` (`conversation_id`).
//! - `step_update` — `step_update.step_type` is `user_input`, `agent_response`
//!   (`text_delta`, and `usage` on the step's `DONE`), `tool` (`tool_name`,
//!   `tool_info.parameters`, and `tool_info.output` once `DONE`) or
//!   `system_message`; `state` is `ACTIVE`, `DONE` or `ERROR`.
//! - `result` — `result.response` (the final answer), `status`, `usage`.
//!
//! Terminal output carries `\r\n`; the carriage returns are dropped.

use serde_json::Value;

use super::StreamParser;
use crate::agent::ui::AgentUi;

#[derive(Debug, Default)]
pub struct AgyStream {
    segment: String,
    /// The step index of the shell command in flight, if one is.
    open_command: Option<u64>,
    response: Option<String>,
    error: Option<String>,
    saw_events: bool,
}

impl StreamParser for AgyStream {
    fn on_line(&mut self, line: &str, ui: &mut dyn AgentUi) {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            return;
        };
        let Some(kind) = v.get("event").and_then(Value::as_str) else {
            return;
        };
        self.saw_events = true;
        match kind {
            "step_update" => self.step(v.get("step_update").unwrap_or(&Value::Null), ui),
            "result" => {
                let r = v.get("result").cloned().unwrap_or(Value::Null);
                if let Some(u) = r.get("usage") {
                    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                    ui.tokens(
                        n("input_tokens") + n("cache_read_tokens"),
                        n("output_tokens"),
                    );
                }
                self.response = r
                    .get("response")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let status = r.get("status").and_then(Value::as_str).unwrap_or("");
                if !status.is_empty() && status != "SUCCESS" {
                    self.error = Some(format!("agy finished with status {status}"));
                }
            }
            _ => {}
        }
    }

    fn saw_events(&self) -> bool {
        self.saw_events
    }

    fn answer(&self) -> String {
        self.response
            .clone()
            .unwrap_or_else(|| self.segment.clone())
            .trim()
            .to_string()
    }

    fn error(&self) -> Option<String> {
        self.error.clone()
    }
}

impl AgyStream {
    fn step(&mut self, s: &Value, ui: &mut dyn AgentUi) {
        let state = s.get("state").and_then(Value::as_str).unwrap_or("");
        let index = s.get("step_index").and_then(Value::as_u64);
        match s.get("step_type").and_then(Value::as_str) {
            Some("agent_response") => {
                if let Some(t) = s.get("text_delta").and_then(Value::as_str) {
                    let t = t.replace('\r', "");
                    ui.model_delta(&t);
                    self.segment.push_str(&t);
                }
                if state == "DONE" {
                    ui.model_done();
                }
            }
            Some("tool") => {
                let name = s.get("tool_name").and_then(Value::as_str).unwrap_or("tool");
                let info = s.get("tool_info").cloned().unwrap_or(Value::Null);
                let params = info.get("parameters").cloned().unwrap_or(Value::Null);
                let is_shell = name == "run_command";
                match state {
                    "ACTIVE" => {
                        self.segment.clear();
                        if is_shell {
                            let cmd = params
                                .get("CommandLine")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            ui.command_start(cmd);
                            self.open_command = index;
                        } else {
                            ui.tool_use(&summarize(name, &params));
                        }
                    }
                    "DONE" | "ERROR" if is_shell && self.open_command == index => {
                        let out = info
                            .get("output")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .replace('\r', "");
                        if !out.is_empty() {
                            ui.command_output(&out);
                        }
                        ui.command_end(i32::from(state == "ERROR"), "");
                        self.open_command = None;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn summarize(name: &str, params: &Value) -> String {
    let arg = ["TargetFile", "AbsolutePath", "SearchPath", "Query", "Url"]
        .iter()
        .find_map(|k| params.get(*k).and_then(Value::as_str));
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
        let mut s = AgyStream::default();
        let mut ui = Rec::default();
        for line in include_str!("testdata/agy-1.2.9.jsonl").lines() {
            s.on_line(line, &mut ui);
        }
        assert!(s.saw_events());
        assert_eq!(ui.commands, vec![("ls".to_string(), 0)]);
        assert!(
            ui.tools.iter().any(|t| t.starts_with("write_to_file")),
            "{:?}",
            ui.tools
        );
        assert_eq!(s.answer(), "DONE");
        assert!(s.error().is_none());
        assert!(ui.tokens.unwrap().1 > 0);
    }
}
