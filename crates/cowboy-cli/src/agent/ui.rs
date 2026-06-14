//! The agent's view of a front-end. The loop drives an `AgentUi`; the console
//! one-shot UI prints to stdout, and the ratatui TUI implements the same trait.

use std::io::Write;

/// Events the agent loop reports to a front-end. Methods take `&mut self` and
/// run on the loop's task (no `Send` requirement on the UI).
pub trait AgentUi {
    /// A streamed token of model output.
    fn model_delta(&mut self, text: &str);
    /// The model finished a turn (flush the streamed line).
    fn model_done(&mut self) {}
    /// A shell command is starting.
    fn command_start(&mut self, command: &str);
    /// The command finished with this exit code (output already shown).
    fn command_end(&mut self, exit_code: i32, output: &str);
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
}

impl ConsoleUi {
    pub fn new() -> Self {
        Self::default()
    }
}

impl AgentUi for ConsoleUi {
    fn model_delta(&mut self, text: &str) {
        if !self.streaming {
            print!("\n\x1b[36m"); // cyan
            self.streaming = true;
        }
        print!("{text}");
        let _ = std::io::stdout().flush();
    }

    fn model_done(&mut self) {
        if self.streaming {
            println!("\x1b[0m");
            self.streaming = false;
            let _ = std::io::stdout().flush();
        }
    }

    fn command_start(&mut self, command: &str) {
        println!("\x1b[1;33m$ {command}\x1b[0m");
    }

    fn command_end(&mut self, exit_code: i32, output: &str) {
        print!("{output}");
        if !output.ends_with('\n') {
            println!();
        }
        if exit_code != 0 {
            println!("\x1b[31m[exit {exit_code}]\x1b[0m");
        }
    }

    fn final_message(&mut self, message: &str) {
        println!("\n\x1b[1;32m✓ {message}\x1b[0m");
    }

    fn ask_user(&mut self, question: &str) -> String {
        use std::io::BufRead;
        println!("\x1b[1;35m? {question}\x1b[0m");
        print!("> ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        line.trim().to_string()
    }

    fn notice(&mut self, msg: &str) {
        eprintln!("\x1b[2m{msg}\x1b[0m");
    }
}
