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
    /// Running session token estimate (input/output). Default: ignored.
    fn tokens(&mut self, _input: u64, _output: u64) {}
    /// The agent finished with a final summary.
    fn final_message(&mut self, message: &str);
    /// Ask the user a question; return their answer.
    fn ask_user(&mut self, question: &str) -> String;
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

    fn final_message(&mut self, message: &str) {
        if self.final_only {
            // Plain final answer for machine capture (subagent result).
            println!("{message}");
        } else {
            println!("\n\x1b[1;32m✓ {message}\x1b[0m");
        }
    }

    fn ask_user(&mut self, question: &str) -> String {
        // A subagent is non-interactive; don't block on input.
        if self.final_only {
            return String::new();
        }
        use std::io::BufRead;
        println!("\x1b[1;35m? {question}\x1b[0m");
        print!("> ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        line.trim().to_string()
    }

    fn notice(&mut self, msg: &str) {
        if self.final_only {
            return;
        }
        eprintln!("\x1b[2m{msg}\x1b[0m");
    }
}
