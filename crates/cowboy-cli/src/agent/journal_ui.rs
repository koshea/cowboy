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

use super::jobctl::{Answer, ControlDir, Question};
use super::ui::AgentUi;

/// How long a subagent waits for its foreman to answer a question.
///
/// Generous, because the foreman only reads its jobs at an iteration boundary and may be
/// mid-turn on something else. Bounded, because the alternative to giving up is a worker
/// blocked forever on a question nobody is going to answer — and the pre-existing
/// behaviour (proceed with no answer) is a survivable outcome, so waiting past the point
/// of usefulness buys nothing.
const ANSWER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// How often to check for the answer. Matches the turn-request poll on the other side.
const ANSWER_POLL: std::time::Duration = std::time::Duration::from_millis(400);

/// Appends `UiEventMsg`s to a subagent's `events.jsonl`.
pub struct JournalUi {
    file: Mutex<Option<std::fs::File>>,
    /// The control channel to the foreman, when this worker was given one.
    control: Option<ControlDir>,
    /// Questions asked so far, for the request/answer sequence numbers.
    asked: u32,
    /// How long to wait for an answer. A field so a test can exercise the give-up path
    /// without waiting ten minutes for it.
    answer_timeout: std::time::Duration,
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
            // The same channel the turn-request path uses. Absent for an unsupervised
            // worker, which then behaves exactly as before.
            control: ControlDir::from_env(),
            asked: 0,
            answer_timeout: ANSWER_TIMEOUT,
        }
    }

    /// Use `control` as the channel to the foreman instead of the environment's.
    #[cfg(test)]
    fn with_control(mut self, control: ControlDir, answer_timeout: std::time::Duration) -> Self {
        self.control = Some(control);
        self.answer_timeout = answer_timeout;
        self
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

    /// Put `question` to the foreman and wait for a reply. `None` if there is no channel,
    /// or nobody answered in time.
    ///
    /// Blocking, matching the `AgentUi::ask_user` contract that every other implementation
    /// already follows (`SocketUi` blocks on a channel the same way).
    fn ask_foreman(&mut self, question: &str, options: &[String]) -> Option<String> {
        let dir = self.control.clone()?;
        self.asked += 1;
        let seq = self.asked;
        let q = Question {
            seq,
            question: question.to_string(),
            options: options.to_vec(),
        };
        if dir.write_question(&q).is_err() {
            return None;
        }
        self.emit(UiEventMsg::Notice(format!(
            "⏸ asked the foreman: {question}"
        )));

        let deadline = std::time::Instant::now() + self.answer_timeout;
        while std::time::Instant::now() < deadline {
            if let Some(Answer { answer, .. }) = dir.read_answer(seq) {
                self.emit(UiEventMsg::Notice(format!("▶ foreman answered: {answer}")));
                return Some(answer);
            }
            std::thread::sleep(ANSWER_POLL);
        }
        self.emit(UiEventMsg::Notice(
            "no answer from the foreman — proceeding without one".to_string(),
        ));
        None
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
    fn context_usage(&mut self, u: &crate::agent::ui::ContextUsage) {
        self.emit(UiEventMsg::ContextUsage {
            used: u.used,
            budget: u.budget,
            window: u.window,
            reserve: u.reserve,
            top: u.top.clone(),
        });
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
    fn subagent_pending(&mut self, label: &str, model: &str, id: &str) {
        self.emit(UiEventMsg::SubagentPending {
            label: label.to_string(),
            model: model.to_string(),
            id: id.to_string(),
        });
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
    fn jobs_changed(&mut self, jobs: &[cowboy_core::daemonproto::JobInfo]) {
        self.emit(UiEventMsg::JobsChanged(jobs.to_vec()));
    }
    fn queue_changed(&mut self, pending: &[String]) {
        self.emit(UiEventMsg::QueueChanged {
            pending: pending.to_vec(),
        });
    }
    fn steering(&mut self, text: &str) {
        self.emit(UiEventMsg::SteerDelivered(text.to_string()));
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
    fn ask_user(&mut self, question: &str, options: &[String]) -> String {
        // A subagent has no terminal, but it does have a foreman — and the foreman is
        // exactly who a question like "which of these two APIs did you mean?" is for.
        // Before this, the answer was unconditionally "" ("proceed"), so a worker that hit
        // a real ambiguity guessed, and the guess surfaced as a confidently wrong result.
        //
        // Still fails open: no channel, or no answer in time, and we return "" as before.
        // A blocked worker is worse than a worker that proceeded on its own judgement.
        self.ask_foreman(question, options).unwrap_or_default()
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

    /// A control dir in a temp location, without touching the real state dir.
    fn control() -> (assert_fs::TempDir, ControlDir) {
        let tmp = assert_fs::TempDir::new().unwrap();
        let path = tmp.path().join("jobs/parent/job");
        std::fs::create_dir_all(&path).unwrap();
        (tmp, ControlDir::at(path))
    }

    #[test]
    fn a_subagent_with_no_channel_proceeds_as_before() {
        // The pre-existing behaviour, and still the fallback: an unsupervised worker has
        // nobody to ask, and blocking it on a question would be worse than guessing.
        let tmp = assert_fs::TempDir::new().unwrap();
        let mut ui = JournalUi::new(&tmp.path().join("events.jsonl"));
        ui.control = None;
        assert_eq!(ui.ask_user("which database?", &[]), "");
    }

    #[test]
    fn a_question_reaches_the_foreman_and_its_answer_comes_back() {
        let (_tmp, dir) = control();
        let jtmp = assert_fs::TempDir::new().unwrap();
        let journal = jtmp.path().join("events.jsonl");

        // The foreman side: wait for the question, then answer it.
        let answering = dir.clone();
        let foreman = std::thread::spawn(move || {
            for _ in 0..200 {
                if let Some(q) = answering.read_question(1) {
                    answering
                        .write_answer(&Answer {
                            seq: q.seq,
                            answer: "postgres — sqlite is only for the tests".into(),
                        })
                        .unwrap();
                    return q.question;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            panic!("the question never arrived");
        });

        let mut ui = JournalUi::new(&journal).with_control(dir, std::time::Duration::from_secs(10));
        let answer = ui.ask_user("which database?", &["postgres".into(), "sqlite".into()]);
        assert_eq!(answer, "postgres — sqlite is only for the tests");
        assert_eq!(foreman.join().unwrap(), "which database?");

        // The exchange is journaled, so watching the subagent shows why it paused rather
        // than an unexplained gap.
        let log = std::fs::read_to_string(&journal).unwrap();
        assert!(log.contains("asked the foreman"), "{log}");
        assert!(log.contains("foreman answered"), "{log}");
    }

    #[test]
    fn an_unanswered_question_gives_up_and_proceeds() {
        // Fail-open on purpose: a worker blocked forever on a question nobody will answer
        // is a worse outcome than one that used its own judgement, which is what it did
        // before this channel existed.
        let (_tmp, dir) = control();
        let jtmp = assert_fs::TempDir::new().unwrap();
        let journal = jtmp.path().join("events.jsonl");
        let mut ui = JournalUi::new(&journal)
            .with_control(dir.clone(), std::time::Duration::from_millis(300));

        let started = std::time::Instant::now();
        assert_eq!(ui.ask_user("proceed?", &[]), "");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        // It still filed the question, so a foreman that shows up late can see what was
        // asked even though the worker moved on.
        assert!(dir.read_question(1).is_some());
        assert!(std::fs::read_to_string(&journal)
            .unwrap()
            .contains("no answer from the foreman"));
    }

    #[test]
    fn a_second_question_uses_a_new_sequence() {
        // Or the stale first answer would resolve it instantly with the wrong reply.
        let (_tmp, dir) = control();
        let jtmp = assert_fs::TempDir::new().unwrap();
        let mut ui = JournalUi::new(&jtmp.path().join("events.jsonl"))
            .with_control(dir.clone(), std::time::Duration::from_millis(200));

        dir.write_answer(&Answer {
            seq: 1,
            answer: "first".into(),
        })
        .unwrap();
        assert_eq!(ui.ask_user("q1", &[]), "first");
        // No answer filed for seq 2, so this one times out rather than reusing "first".
        assert_eq!(ui.ask_user("q2", &[]), "");
        assert_eq!(dir.read_question(2).unwrap().question, "q2");
    }
}
