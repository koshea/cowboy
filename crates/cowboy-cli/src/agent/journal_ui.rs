//! `JournalUi` — the `AgentUi` used by a **subagent** child process. It appends
//! every display event to its session's `events.jsonl` (one [`UiEventMsg`] per
//! line, identical to [`super::socket_ui::SocketUi`]'s journal) so a parent/UI can
//! *tail* a running subagent live, and it still prints the final answer to stdout
//! so the spawning parent captures the result from the child's output — preserving
//! the old `COWBOY_PRINT_FINAL_ONLY` behavior. Unlike `SocketUi` there is no
//! socket or broadcast: subagents aren't attachable, they're watched via the file.

use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use cowboy_core::daemonproto::UiEventMsg;

use super::ui::AgentUi;

/// Appends `UiEventMsg`s to a subagent's `events.jsonl`.
pub struct JournalUi {
    file: Mutex<Option<std::fs::File>>,
}

impl JournalUi {
    /// Open (create/append) the journal at `journal_path`. A failure to open is
    /// non-fatal — the subagent still runs, it just isn't watchable.
    pub fn new(journal_path: &Path) -> Self {
        if let Some(parent) = journal_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(journal_path)
            .ok();
        Self {
            file: Mutex::new(file),
        }
    }

    /// Append one event as a JSON line (best-effort; a write error just drops it).
    fn emit(&self, event: UiEventMsg) {
        let mut guard = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(f) = guard.as_mut() {
            let line = serde_json::to_string(&event).unwrap_or_default();
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

impl AgentUi for JournalUi {
    fn model_delta(&mut self, text: &str) {
        self.emit(UiEventMsg::Delta(text.to_string()));
    }
    fn model_reasoning(&mut self, text: &str) {
        self.emit(UiEventMsg::Reasoning(text.to_string()));
    }
    fn model_done(&mut self) {
        self.emit(UiEventMsg::ModelDone);
    }
    fn command_start(&mut self, command: &str) {
        self.emit(UiEventMsg::CommandStart(command.to_string()));
    }
    fn command_output(&mut self, chunk: &str) {
        self.emit(UiEventMsg::CommandOutput(chunk.to_string()));
    }
    fn command_end(&mut self, exit_code: i32, output: &str) {
        self.emit(UiEventMsg::CommandEnd {
            code: exit_code,
            output: output.to_string(),
        });
    }
    fn tool_use(&mut self, summary: &str) {
        self.emit(UiEventMsg::ToolUse(summary.to_string()));
    }
    fn file_diff(&mut self, path: &str, diff: &str) {
        self.emit(UiEventMsg::FileDiff {
            path: path.to_string(),
            diff: diff.to_string(),
        });
    }
    fn tokens(&mut self, input: u64, output: u64) {
        self.emit(UiEventMsg::Tokens { input, output });
    }
    fn cost(&mut self, usd: f64) {
        self.emit(UiEventMsg::Cost(usd));
    }
    fn blocked(&mut self, reason: Option<&str>) {
        self.emit(UiEventMsg::Blocked(reason.map(str::to_string)));
    }
    fn plan(&mut self, steps: &[(String, String)]) {
        self.emit(UiEventMsg::Plan(steps.to_vec()));
    }
    fn subagent_started(&mut self, label: &str, model: &str, id: &str) {
        self.emit(UiEventMsg::SubagentStarted {
            label: label.to_string(),
            model: model.to_string(),
            id: id.to_string(),
        });
    }
    fn subagent_done(&mut self, label: &str, ok: bool, id: &str) {
        self.emit(UiEventMsg::SubagentDone {
            label: label.to_string(),
            ok,
            id: id.to_string(),
        });
    }
    fn final_message(&mut self, message: &str) {
        // Journal it AND print to stdout: the parent captures the subagent's
        // result from the child's stdout (the `COWBOY_PRINT_FINAL_ONLY` contract).
        self.emit(UiEventMsg::Final(message.to_string()));
        println!("{message}");
    }
    fn notice(&mut self, msg: &str) {
        self.emit(UiEventMsg::Notice(msg.to_string()));
    }
    fn ask_user(&mut self, _question: &str, _options: &[String]) -> String {
        // A subagent is non-interactive — no one can answer. Empty = "proceed".
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journals_events_as_jsonl_and_keeps_final() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let path = tmp.path().join("events.jsonl");
        {
            let mut ui = JournalUi::new(&path);
            ui.command_start("cargo test");
            ui.subagent_started("docs", "cheap", "child-1");
            ui.final_message("done");
        }
        let lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(lines.len(), 3);
        // Each line round-trips to a UiEventMsg.
        for l in &lines {
            serde_json::from_str::<UiEventMsg>(l).unwrap();
        }
        assert!(lines[0].contains("command_start"));
        assert!(lines[1].contains("child-1"));
        assert!(lines[2].contains("\"final\""));
    }
}
