//! The agent's view of a front-end. The loop drives an `AgentUi`; the console
//! one-shot UI prints to stdout, and the ratatui TUI implements the same trait.

use std::io::Write;

/// Events the agent loop reports to a front-end. Methods take `&mut self` and
/// run on the loop's task (no `Send` requirement on the UI).
pub trait AgentUi {
    /// A streamed token of model output.
    fn model_delta(&mut self, text: &str);
    /// A streamed token of the model's "thinking" (reasoning). Default: ignored.
    fn model_reasoning(&mut self, _text: &str) {}
    /// The model finished a turn (flush the streamed line).
    fn model_done(&mut self) {}
    /// A shell command is starting.
    fn command_start(&mut self, command: &str);
    /// A chunk of live command output (streamed as it arrives).
    fn command_output(&mut self, _chunk: &str) {}
    /// The command finished with this exit code (output already streamed).
    fn command_end(&mut self, exit_code: i32, output: &str);
    /// A structured file action (read/edit/write) was performed.
    fn tool_use(&mut self, summary: &str) {
        self.notice(summary);
    }
    /// A file was created or edited: `diff` is a unified diff of the change,
    /// shown with +/- coloring beneath the tool summary. Default: ignored.
    fn file_diff(&mut self, _path: &str, _diff: &str) {}
    /// Running session token estimate (input/output). Default: ignored.
    fn tokens(&mut self, _input: u64, _output: u64) {}
    /// Running estimated session spend in USD. Default: ignored.
    fn cost(&mut self, _usd: f64) {}
    /// The agent's working plan changed: ordered (step, status) pairs where
    /// status is "pending" | "in_progress" | "done". Default: ignored.
    fn plan(&mut self, _steps: &[(String, String)]) {}
    /// The session declared itself blocked (`Some(reason)`) or unblocked
    /// (`None`). Default: ignored.
    fn blocked(&mut self, _reason: Option<&str>) {}
    /// A crew subagent was dispatched (`label` = routing label, `model` =
    /// resolved model, `id` = the subagent's session id, whose live journal the
    /// UI can watch). Default: ignored.
    fn subagent_started(&mut self, _label: &str, _model: &str, _id: &str) {}
    /// A crew subagent finished (`ok` = whether it produced a result; `id`
    /// correlates to the start). Default: ignored.
    fn subagent_done(&mut self, _label: &str, _ok: bool, _id: &str) {}
    /// The agent finished with a final summary.
    fn final_message(&mut self, message: &str);
    /// Ask the user a question and return their answer. `options` (possibly
    /// empty) are suggested choices; the user may still answer freely.
    fn ask_user(&mut self, question: &str, options: &[String]) -> String;
    /// A general notice (errors, status).
    fn notice(&mut self, msg: &str);
}

/// Simple console UI for one-shot / non-TUI runs.
#[derive(Default)]
pub struct ConsoleUi {
    streaming: bool,
    reasoning: bool,
    /// When set (subagent runs via `COWBOY_PRINT_FINAL_ONLY`), suppress all
    /// intermediate output and print only the final message — so the caller can
    /// capture the subagent's answer from stdout.
    final_only: bool,
}

impl ConsoleUi {
    pub fn new() -> Self {
        Self {
            final_only: std::env::var("COWBOY_PRINT_FINAL_ONLY").is_ok(),
            ..Default::default()
        }
    }
}

impl AgentUi for ConsoleUi {
    fn model_delta(&mut self, text: &str) {
        if self.final_only {
            return;
        }
        if self.reasoning {
            print!("\x1b[0m"); // close the dim "thinking" block
            self.reasoning = false;
        }
        if !self.streaming {
            print!("\n\x1b[36m"); // cyan
            self.streaming = true;
        }
        print!("{text}");
        let _ = std::io::stdout().flush();
    }

    fn model_reasoning(&mut self, text: &str) {
        if self.final_only {
            return;
        }
        if !self.reasoning {
            print!("\n\x1b[2m"); // dim
            self.reasoning = true;
        }
        print!("{text}");
        let _ = std::io::stdout().flush();
    }

    fn model_done(&mut self) {
        if self.streaming || self.reasoning {
            println!("\x1b[0m");
            self.streaming = false;
            self.reasoning = false;
            let _ = std::io::stdout().flush();
        }
    }

    fn command_start(&mut self, command: &str) {
        if self.final_only {
            return;
        }
        println!("\x1b[1;33m$ {command}\x1b[0m");
    }

    fn command_output(&mut self, chunk: &str) {
        if self.final_only {
            return;
        }
        print!("{chunk}");
        let _ = std::io::stdout().flush();
    }

    fn command_end(&mut self, exit_code: i32, _output: &str) {
        // Output was already streamed via command_output.
        if self.final_only {
            return;
        }
        if exit_code != 0 {
            println!("\x1b[31m[exit {exit_code}]\x1b[0m");
        }
    }

    fn tool_use(&mut self, summary: &str) {
        if self.final_only {
            return;
        }
        println!("\x1b[35m✎ {summary}\x1b[0m");
    }

    fn file_diff(&mut self, _path: &str, diff: &str) {
        if self.final_only {
            return;
        }
        for line in diff.lines() {
            let color = match line.as_bytes().first() {
                Some(b'+') => "\x1b[32m",                  // green
                Some(b'-') => "\x1b[31m",                  // red
                _ if line.starts_with("@@") => "\x1b[36m", // cyan
                _ => "\x1b[2m",                            // dim context
            };
            println!("{color}{line}\x1b[0m");
        }
    }

    fn plan(&mut self, steps: &[(String, String)]) {
        if self.final_only || steps.is_empty() {
            return;
        }
        println!("\x1b[36m▣ plan\x1b[0m");
        for (step, status) in steps {
            let (mark, color) = match status.as_str() {
                "done" => ("✓", "\x1b[32m"),
                "in_progress" => ("▸", "\x1b[33m"),
                _ => ("·", "\x1b[2m"),
            };
            println!("{color}  {mark} {step}\x1b[0m");
        }
        let _ = std::io::stdout().flush();
    }

    fn blocked(&mut self, reason: Option<&str>) {
        if self.final_only {
            return;
        }
        match reason {
            Some(r) => println!("\x1b[1;33m⏸ blocked: {r}\x1b[0m"),
            None => println!("\x1b[1;32m▶ unblocked\x1b[0m"),
        }
    }

    fn final_message(&mut self, message: &str) {
        if self.final_only {
            // Plain final answer for machine capture (subagent result).
            println!("{message}");
        } else {
            println!("\n\x1b[1;32m✓ {message}\x1b[0m");
        }
    }

    fn ask_user(&mut self, question: &str, options: &[String]) -> String {
        // Non-interactive (a subagent, or stdin isn't a terminal — piped/headless):
        // no one can answer, so don't block on input — return empty (the agent
        // treats this as "no answer / proceed").
        use std::io::IsTerminal;
        if self.final_only || !std::io::stdin().is_terminal() {
            return String::new();
        }
        use std::io::BufRead;
        println!("\x1b[1;35m? {question}\x1b[0m");
        for (i, opt) in options.iter().enumerate() {
            println!("  \x1b[36m{}\x1b[0m {opt}", i + 1);
        }
        if options.is_empty() {
            print!("> ");
        } else {
            print!("pick 1-{} or type an answer > ", options.len());
        }
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        let answer = line.trim().to_string();
        // A bare number selects the matching option.
        if let Ok(n) = answer.parse::<usize>() {
            if n >= 1 && n <= options.len() {
                return options[n - 1].clone();
            }
        }
        answer
    }

    fn notice(&mut self, msg: &str) {
        if self.final_only {
            return;
        }
        eprintln!("\x1b[2m{msg}\x1b[0m");
    }
}
