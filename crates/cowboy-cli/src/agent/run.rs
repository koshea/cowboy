//! The Cowboy-owned agent loop: model turn -> tool call -> observation ->
//! repeat, until `final`, `ask_user` is answered, or limits are hit. Cowboy
//! owns this lifecycle; no agent framework.

use anyhow::Result;
use cowboy_core::config::AgentBehavior;
use cowboy_core::model::{ChatResponse, Message, ModelClient, Role, ToolDef};
use tokio_util::sync::CancellationToken;

use super::tools::{
    self, AskUserArgs, EditArgs, FinalArgs, ReadArgs, ShellArgs, SubagentArgs, WriteArgs,
};
use super::ui::AgentUi;
use crate::net::docker::ExecResult;
use crate::net::runtime::AgentRuntime;
use crate::session::SessionLogger;

/// Default agent system prompt (see plan §10.3).
pub const SYSTEM_PROMPT: &str = "\
You are Cowboy, an autonomous coding agent running inside a Docker container.

The project is mounted at /workspace. You may freely inspect, edit, build, test, \
and run code inside the container. Use `shell` for builds, tests, git, and other \
commands. For files, prefer the structured tools: `read` (with line numbers), \
`edit` (exact unique-string replacement), and `write` (create/overwrite) — they \
are more reliable and cheaper than `cat`/`sed`/heredocs.

Cowboy-specific helpers are CLIs you invoke through `shell`, e.g. `cowboy patch \
show` and `cowboy proc start <name>`. You do not need to ask before ordinary \
development actions inside the container.

Reusable skills may be available: run `cowboy skill list` to see them and \
`cowboy skill show <name>` to read a skill's instructions, then follow them. \
For a large, independent sub-task, use the `subagent` tool to delegate it.

The runtime enforces network, host, and secret permissions outside your control. \
Outbound network access goes through a gateway that allows, denies, or prompts \
the user per destination. A blocked request surfaces as a connection/TLS error \
(e.g. \"connection reset\", \"TLS closed\", curl exit 35/35) — this means the \
host has not approved that destination, NOT that the destination is down. Do not \
retry the same blocked host with different tools or flags; instead state plainly \
which host:port you need and why, and let the user approve it (or proceed without \
network). If a command cannot access something, observe the failure and continue.

Before large edits, inspect the repository and form a brief plan. After edits, run \
relevant checks. When finished, call `final` summarizing what changed, what was \
validated, and remaining risks or follow-up work.";

/// Drives a single agent session.
pub struct AgentLoop<'a> {
    model: Box<dyn ModelClient>,
    runtime: AgentRuntime,
    tools: Vec<ToolDef>,
    behavior: AgentBehavior,
    cancel: CancellationToken,
    /// Model context window (tokens) for history pruning.
    context_window: usize,
    pruned_notified: bool,
    /// Recursion depth for subagents (0 = top-level).
    subagent_depth: usize,
    /// The most recent turn's final message (for the session summary).
    last_final: Option<String>,
    /// Running session token estimates (tiktoken-based; provider-independent).
    tokens_in: u64,
    tokens_out: u64,
    messages: Vec<Message>,
    ui: &'a mut dyn AgentUi,
    logger: Option<SessionLogger>,
}

/// Tokens reserved for the model's response + tool schemas when budgeting.
const RESPONSE_HEADROOM: usize = 4096;
/// Maximum subagent nesting depth (prevents runaway recursion).
const MAX_SUBAGENT_DEPTH: usize = 2;

impl<'a> AgentLoop<'a> {
    pub fn new(
        model: Box<dyn ModelClient>,
        runtime: AgentRuntime,
        behavior: AgentBehavior,
        context_window: usize,
        cancel: CancellationToken,
        ui: &'a mut dyn AgentUi,
    ) -> Self {
        Self {
            model,
            runtime,
            tools: tools::definitions(),
            behavior,
            cancel,
            context_window,
            pruned_notified: false,
            subagent_depth: std::env::var("COWBOY_SUBAGENT_DEPTH")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            last_final: None,
            tokens_in: 0,
            tokens_out: 0,
            messages: vec![Message::system(SYSTEM_PROMPT)],
            ui,
            logger: None,
        }
    }

    /// Accumulate per-call token estimates (prompt sent + completion received)
    /// and report the running session total to the UI. Estimates use the local
    /// tokenizer, so they are provider-independent and roughly track billing.
    fn account_tokens(&mut self, prompt_est: u64, response: &ChatResponse) {
        self.tokens_in += prompt_est;
        let mut out =
            cowboy_core::tokens::count(response.content.as_deref().unwrap_or_default()) as u64;
        for tc in &response.tool_calls {
            out += (cowboy_core::tokens::count(&tc.arguments)
                + cowboy_core::tokens::count(&tc.name)) as u64;
        }
        self.tokens_out += out;
        self.ui.tokens(self.tokens_in, self.tokens_out);
    }

    /// Approximate token count of a message (content + tool-call arguments).
    fn message_tokens(m: &Message) -> usize {
        let mut n = cowboy_core::tokens::count(&m.content) + 4;
        for tc in &m.tool_calls {
            n += cowboy_core::tokens::count(&tc.arguments)
                + cowboy_core::tokens::count(&tc.name)
                + 4;
        }
        n
    }

    /// Prune the oldest history to keep the conversation within the context
    /// window (minus response headroom). The system message is always kept, and
    /// we never leave an orphan tool-result at the front.
    fn fit_context(&mut self) {
        let budget = self.context_window.saturating_sub(RESPONSE_HEADROOM);
        if budget == 0 {
            return;
        }
        let mut pruned = false;
        while self.messages.len() > 2 {
            let total: usize = self.messages.iter().map(Self::message_tokens).sum();
            if total <= budget {
                break;
            }
            self.messages.remove(1);
            while self.messages.len() > 1 && self.messages[1].role == Role::Tool {
                self.messages.remove(1);
            }
            pruned = true;
        }
        if pruned && !self.pruned_notified {
            self.pruned_notified = true;
            self.ui.notice("context window full; pruned older history");
        }
    }

    /// Attach a session logger (records transcript, commands, final summary).
    pub fn with_logger(mut self, logger: Option<SessionLogger>) -> Self {
        self.logger = logger;
        self
    }

    /// Swap the model client (and its context window) mid-session, keeping the
    /// conversation. Used by the `/model` command.
    pub fn set_model(&mut self, model: Box<dyn ModelClient>, context_window: usize) {
        self.model = model;
        self.context_window = context_window;
    }

    /// Run one conversational turn for `task`, keeping the conversation (and the
    /// session logger) alive for subsequent turns. `turn_cancel` interrupts just
    /// this turn. Does NOT finalize the session.
    pub async fn run_turn(
        &mut self,
        task: &str,
        turn_cancel: CancellationToken,
    ) -> Result<Option<String>> {
        self.cancel = turn_cancel;
        let outcome = self.run_inner(task).await;
        if let Ok(Some(m)) = &outcome {
            self.last_final = Some(m.clone());
        }
        outcome
    }

    /// Finalize the session log (diff + summary). Call once when the
    /// conversation ends.
    pub fn finalize_session(&self) {
        if let Some(l) = &self.logger {
            l.finalize(self.last_final.as_deref());
        }
    }

    /// The host project root (workspace bind-mount source).
    pub fn root(&self) -> &std::path::Path {
        self.runtime.root()
    }

    /// One-shot convenience: run a single turn then finalize (console mode/tests).
    pub async fn run(&mut self, task: &str) -> Result<Option<String>> {
        let cancel = self.cancel.clone();
        let outcome = self.run_turn(task, cancel).await;
        self.finalize_session();
        outcome
    }

    /// Run the loop for `task` until completion, cancellation, or the iteration
    /// cap. Returns the final message if the agent produced one.
    async fn run_inner(&mut self, task: &str) -> Result<Option<String>> {
        let user_msg = Message::user(task);
        if let Some(l) = &mut self.logger {
            l.log_message(&user_msg);
        }
        self.messages.push(user_msg);

        for _ in 0..self.behavior.max_iterations {
            if self.cancel.is_cancelled() {
                self.ui.notice("interrupted");
                return Ok(None);
            }

            // Keep history within the model's context window.
            self.fit_context();

            // Estimate the prompt tokens actually sent (post-pruning).
            let prompt_est: u64 = self
                .messages
                .iter()
                .map(Self::message_tokens)
                .sum::<usize>() as u64;
            let response = match self.call_model().await {
                Ok(r) => r,
                Err(_) if self.cancel.is_cancelled() => {
                    self.ui.notice("interrupted");
                    return Ok(None);
                }
                Err(e) => {
                    self.ui.notice(&format!("model error: {e}"));
                    return Err(e);
                }
            };
            self.account_tokens(prompt_est, &response);

            // Record the assistant turn (content + any tool calls).
            let assistant = Message {
                role: Role::Assistant,
                content: response.content.clone().unwrap_or_default(),
                tool_call_id: None,
                tool_calls: response.tool_calls.clone(),
            };
            if let Some(l) = &mut self.logger {
                l.log_message(&assistant);
            }
            self.messages.push(assistant);

            if response.tool_calls.is_empty() {
                // No tool call: treat any content as an implicit final answer.
                let msg = response.content.unwrap_or_default();
                if !msg.is_empty() {
                    self.ui.final_message(&msg);
                    return Ok(Some(msg));
                }
                self.ui.notice("model produced no action; stopping");
                return Ok(None);
            }

            if let Some(final_msg) = self.handle_tool_calls(&response).await? {
                return Ok(Some(final_msg));
            }
        }

        self.ui.notice(&format!(
            "reached max_iterations ({})",
            self.behavior.max_iterations
        ));
        Ok(None)
    }

    /// Process this turn's tool calls. Returns `Some(message)` if `final` was
    /// called.
    async fn handle_tool_calls(&mut self, response: &ChatResponse) -> Result<Option<String>> {
        for call in &response.tool_calls {
            match call.name.as_str() {
                tools::TOOL_FINAL => {
                    let args: FinalArgs = match parse_args(&call.arguments) {
                        Ok(a) => a,
                        Err(e) => {
                            self.tool_error(&call.id, &call.name, &e.to_string());
                            continue;
                        }
                    };
                    if let Some(l) = &self.logger {
                        l.write_final(&args.message);
                    }
                    self.ui.final_message(&args.message);
                    return Ok(Some(args.message));
                }
                tools::TOOL_SHELL => {
                    let args: ShellArgs = match parse_args(&call.arguments) {
                        Ok(a) => a,
                        Err(e) => {
                            self.tool_error(&call.id, &call.name, &e.to_string());
                            continue;
                        }
                    };
                    self.ui.command_start(&args.command);
                    let started = std::time::Instant::now();
                    let (result, output) = self.run_shell_streaming(&args).await?;
                    let duration_ms = started.elapsed().as_millis();
                    self.ui.command_end(result.exit_code, "");
                    if let Some(l) = &mut self.logger {
                        l.log_command(&args.command, result.exit_code, duration_ms, &output);
                    }
                    let truncated = truncate(&output, self.behavior.max_command_output_bytes);
                    let observation = format!("[exit code: {}]\n{}", result.exit_code, truncated);
                    let tool_msg = Message::tool_result(&call.id, observation);
                    if let Some(l) = &mut self.logger {
                        l.log_message(&tool_msg);
                    }
                    self.messages.push(tool_msg);
                }
                tools::TOOL_READ => {
                    let args: ReadArgs = match parse_args(&call.arguments) {
                        Ok(a) => a,
                        Err(e) => {
                            self.tool_error(&call.id, &call.name, &e.to_string());
                            continue;
                        }
                    };
                    self.ui.tool_use(&format!("read {}", args.path));
                    let payload = serde_json::json!({
                        "op": "read", "path": args.path,
                        "offset": args.offset, "limit": args.limit,
                    });
                    self.run_fileop(&call.id, &payload).await?;
                }
                tools::TOOL_EDIT => {
                    let args: EditArgs = match parse_args(&call.arguments) {
                        Ok(a) => a,
                        Err(e) => {
                            self.tool_error(&call.id, &call.name, &e.to_string());
                            continue;
                        }
                    };
                    let payload = serde_json::json!({
                        "op": "edit", "path": args.path,
                        "old": args.old, "new": args.new, "replace_all": args.replace_all,
                    });
                    let (exit, out) = self.run_fileop(&call.id, &payload).await?;
                    self.ui
                        .tool_use(&fileop_summary("edit", &args.path, exit, &out));
                }
                tools::TOOL_WRITE => {
                    let args: WriteArgs = match parse_args(&call.arguments) {
                        Ok(a) => a,
                        Err(e) => {
                            self.tool_error(&call.id, &call.name, &e.to_string());
                            continue;
                        }
                    };
                    let payload = serde_json::json!({
                        "op": "write", "path": args.path, "content": args.content,
                    });
                    let (exit, out) = self.run_fileop(&call.id, &payload).await?;
                    self.ui
                        .tool_use(&fileop_summary("write", &args.path, exit, &out));
                }
                tools::TOOL_ASK_USER => {
                    let args: AskUserArgs = match parse_args(&call.arguments) {
                        Ok(a) => a,
                        Err(e) => {
                            self.tool_error(&call.id, &call.name, &e.to_string());
                            continue;
                        }
                    };
                    let answer = self.ui.ask_user(&args.question);
                    let tool_msg = Message::tool_result(&call.id, answer);
                    if let Some(l) = &mut self.logger {
                        l.log_message(&tool_msg);
                    }
                    self.messages.push(tool_msg);
                }
                tools::TOOL_SUBAGENT => {
                    let args: SubagentArgs = match parse_args(&call.arguments) {
                        Ok(a) => a,
                        Err(e) => {
                            self.tool_error(&call.id, &call.name, &e.to_string());
                            continue;
                        }
                    };
                    let result = self.run_subagent(&args).await;
                    let tool_msg = Message::tool_result(&call.id, result);
                    if let Some(l) = &mut self.logger {
                        l.log_message(&tool_msg);
                    }
                    self.messages.push(tool_msg);
                }
                other => {
                    self.messages.push(Message::tool_result(
                        &call.id,
                        format!("error: unknown tool {other}"),
                    ));
                }
            }
        }
        Ok(None)
    }

    /// Record a tool error as an observation so the model can self-correct.
    fn tool_error(&mut self, id: &str, name: &str, err: &str) {
        let msg = Message::tool_result(
            id,
            format!("error: invalid arguments for `{name}`: {err}; please correct and retry"),
        );
        if let Some(l) = &mut self.logger {
            l.log_message(&msg);
        }
        self.messages.push(msg);
    }

    /// Run a structured file operation in the container, record the observation
    /// for the model, and log it. Returns (exit_code, helper output).
    async fn run_fileop(
        &mut self,
        call_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(i32, String)> {
        let (result, output) = match self.runtime.fileop(&payload.to_string()).await {
            Ok(v) => v,
            Err(e) => {
                let msg = Message::tool_result(call_id, format!("error: {e}"));
                if let Some(l) = &mut self.logger {
                    l.log_message(&msg);
                }
                self.messages.push(msg);
                return Ok((-1, String::new()));
            }
        };
        let observation = if result.exit_code == 0 {
            output.clone()
        } else {
            format!("error: {}", output.trim())
        };
        let observation = truncate(&observation, self.behavior.max_command_output_bytes);
        let tool_msg = Message::tool_result(call_id, observation);
        if let Some(l) = &mut self.logger {
            l.log_message(&tool_msg);
        }
        self.messages.push(tool_msg);
        Ok((result.exit_code, output))
    }

    /// Run a shell command with live streaming to the UI (interruptible via the
    /// turn's cancel token). Returns (exit, full output).
    async fn run_shell_streaming(&mut self, args: &ShellArgs) -> Result<(ExecResult, String)> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let fut = self.runtime.exec_stream(
            &args.command,
            args.cwd.as_deref(),
            self.behavior.command_timeout_seconds,
            self.cancel.clone(),
            tx,
        );
        tokio::pin!(fut);
        loop {
            tokio::select! {
                biased;
                Some(chunk) = rx.recv() => self.ui.command_output(&chunk),
                res = &mut fut => {
                    while let Ok(chunk) = rx.try_recv() {
                        self.ui.command_output(&chunk);
                    }
                    return res;
                }
            }
        }
    }

    /// Stop managed processes in the container (called on session end).
    pub async fn shutdown(&self) {
        let _ = self.runtime.stop_all_processes().await;
    }

    /// Run a subagent by recursively invoking the `cowboy` CLI in one-shot mode,
    /// reusing this session's container (via `COWBOY_CONTAINER_NAME`) so the
    /// subagent shares the workspace and gateway. Returns its final answer.
    async fn run_subagent(&mut self, args: &SubagentArgs) -> String {
        if self.subagent_depth >= MAX_SUBAGENT_DEPTH {
            return format!(
                "error: maximum subagent depth ({MAX_SUBAGENT_DEPTH}) reached; do this work directly"
            );
        }
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => return format!("subagent error: cannot locate cowboy binary: {e}"),
        };
        let task = match &args.context {
            Some(ctx) if !ctx.is_empty() => format!("{ctx}\n\n{}", args.task),
            _ => args.task.clone(),
        };
        self.ui.notice(&format!("↳ subagent: {}", args.task));

        let output = tokio::process::Command::new(exe)
            .arg(&task)
            .current_dir(self.runtime.root())
            .env("COWBOY_CONTAINER_NAME", self.runtime.container_name())
            .env(
                "COWBOY_SUBAGENT_DEPTH",
                (self.subagent_depth + 1).to_string(),
            )
            .env("COWBOY_PRINT_FINAL_ONLY", "1")
            // Don't let the child's logs corrupt the parent TUI/console.
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await;

        match output {
            Ok(o) => {
                self.ui.notice("↳ subagent finished");
                let result = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if result.is_empty() {
                    "subagent produced no final answer".to_string()
                } else {
                    result
                }
            }
            Err(e) => format!("subagent failed to start: {e}"),
        }
    }

    /// Call the model, streaming deltas to the UI, racing cancellation.
    async fn call_model(&mut self) -> Result<ChatResponse> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let fut = self.model.chat(&self.messages, &self.tools, Some(tx));
        tokio::pin!(fut);
        loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => {
                    anyhow::bail!("interrupted");
                }
                Some(piece) = rx.recv() => {
                    self.ui.model_delta(&piece);
                }
                res = &mut fut => {
                    while let Ok(piece) = rx.try_recv() {
                        self.ui.model_delta(&piece);
                    }
                    self.ui.model_done();
                    return res.map_err(Into::into);
                }
            }
        }
    }
}

fn parse_args<T: serde::de::DeserializeOwned>(arguments: &str) -> Result<T> {
    let args = if arguments.trim().is_empty() {
        "{}"
    } else {
        arguments
    };
    serde_json::from_str(args).map_err(|e| anyhow::anyhow!("invalid tool arguments: {e}"))
}

/// A concise one-line summary of a file op for the UI: the helper's status line
/// on success, or `"<action> <path> — failed"` otherwise.
fn fileop_summary(action: &str, path: &str, exit: i32, output: &str) -> String {
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

/// Truncate `output` to at most `max_bytes`, on a char boundary, with a marker.
fn truncate(output: &str, max_bytes: usize) -> String {
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
    use crate::agent::ui::AgentUi;
    use crate::net::docker::{ContainerState, ExecResult, MockDockerCli};
    use cowboy_core::config::{Mount, SecurityConfig};
    use cowboy_core::model::{ChatResponse, ToolCall};
    use std::sync::Mutex;

    /// A model that returns a scripted sequence of responses.
    struct ScriptedModel {
        responses: Mutex<std::collections::VecDeque<ChatResponse>>,
    }
    impl ScriptedModel {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
            }
        }
    }
    #[async_trait::async_trait]
    impl ModelClient for ScriptedModel {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolDef],
            deltas: Option<tokio::sync::mpsc::UnboundedSender<String>>,
        ) -> Result<ChatResponse, cowboy_core::Error> {
            let r = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default();
            if let (Some(tx), Some(c)) = (deltas, &r.content) {
                let _ = tx.send(c.clone());
            }
            Ok(r)
        }
    }

    #[derive(Default)]
    struct RecordingUi {
        commands: Vec<String>,
        finals: Vec<String>,
        notices: Vec<String>,
        tool_uses: Vec<String>,
    }
    impl AgentUi for RecordingUi {
        fn model_delta(&mut self, _text: &str) {}
        fn command_start(&mut self, command: &str) {
            self.commands.push(command.to_string());
        }
        fn command_end(&mut self, _exit_code: i32, _output: &str) {}
        fn tool_use(&mut self, summary: &str) {
            self.tool_uses.push(summary.to_string());
        }
        fn final_message(&mut self, message: &str) {
            self.finals.push(message.to_string());
        }
        fn ask_user(&mut self, _question: &str) -> String {
            "yes".to_string()
        }
        fn notice(&mut self, msg: &str) {
            self.notices.push(msg.to_string());
        }
    }

    fn tool_call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.into(),
        }
    }

    fn runtime_with(docker: MockDockerCli) -> AgentRuntime {
        let tmp = assert_fs::TempDir::new().unwrap();
        let mut security = SecurityConfig {
            container: cowboy_core::config::ContainerConfig {
                image: "img".into(),
                mounts: vec![Mount {
                    source: ".".into(),
                    target: "/workspace".into(),
                    mode: "rw".into(),
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        security.networks.isolated.enabled = false; // no gateway in unit tests
                                                    // Leak the tempdir so it outlives the runtime for the test.
        let root = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        AgentRuntime::new(Box::new(docker), root, security)
    }

    #[tokio::test]
    async fn runs_shell_then_final() {
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_exec_stream()
            .withf(|_n, _w, _u, command, _t, _c, _ch| command.contains("ls"))
            .times(1)
            .returning(|_, _, _, _, _, _, chunks| {
                let _ = chunks.send("file1\nfile2\n".into());
                Ok((ExecResult { exit_code: 0 }, "file1\nfile2\n".into()))
            });

        let model = ScriptedModel::new(vec![
            ChatResponse {
                content: Some("inspecting".into()),
                tool_calls: vec![tool_call("1", "shell", r#"{"command":"ls"}"#)],
            },
            ChatResponse {
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"done; tests pass"}"#)],
            },
        ]);

        let behavior = cowboy_core::config::AgentBehavior::default();
        let cancel = CancellationToken::new();
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            runtime_with(docker),
            behavior,
            200_000,
            cancel,
            &mut ui,
        );
        let final_msg = agent.run("list the files then finish").await.unwrap();

        assert_eq!(final_msg.as_deref(), Some("done; tests pass"));
        assert_eq!(ui.commands, vec!["ls"]);
        assert_eq!(ui.finals, vec!["done; tests pass"]);
    }

    #[tokio::test]
    async fn runs_edit_via_fileop_then_final() {
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_exec_stdin()
            .withf(|_n, _w, _u, argv, stdin| {
                argv == ["cowboy", "x-fileop"]
                    && stdin.contains("\"op\":\"edit\"")
                    && stdin.contains("main.rs")
            })
            .times(1)
            .returning(|_, _, _, _, _| {
                Ok((
                    ExecResult { exit_code: 0 },
                    "edited main.rs: 1 replacement\n".into(),
                ))
            });

        let model = ScriptedModel::new(vec![
            ChatResponse {
                content: None,
                tool_calls: vec![tool_call(
                    "1",
                    "edit",
                    r#"{"path":"main.rs","old":"foo","new":"bar"}"#,
                )],
            },
            ChatResponse {
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"done"}"#)],
            },
        ]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            runtime_with(docker),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let final_msg = agent.run("edit then finish").await.unwrap();
        assert_eq!(final_msg.as_deref(), Some("done"));
        // The UI showed the helper's status line for the edit.
        assert_eq!(ui.tool_uses, vec!["edited main.rs: 1 replacement"]);
    }

    #[tokio::test]
    async fn stops_at_max_iterations() {
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_exec_stream()
            .returning(|_, _, _, _, _, _, _| Ok((ExecResult { exit_code: 0 }, "ok".into())));
        // Model always asks for another shell command -> never finishes.
        let looping = ScriptedModel::new(vec![]);
        // Empty queue returns default (no tool calls) -> would stop early; instead
        // script many shell calls to exercise the cap.
        {
            let mut q = looping.responses.lock().unwrap();
            for i in 0..10 {
                q.push_back(ChatResponse {
                    content: None,
                    tool_calls: vec![tool_call(
                        &i.to_string(),
                        "shell",
                        r#"{"command":"echo hi"}"#,
                    )],
                });
            }
        }
        let behavior = cowboy_core::config::AgentBehavior {
            max_iterations: 3,
            ..Default::default()
        };
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(looping),
            runtime_with(docker),
            behavior,
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let res = agent.run("loop forever").await.unwrap();
        assert!(res.is_none());
        assert!(ui.notices.iter().any(|n| n.contains("max_iterations")));
        assert_eq!(ui.commands.len(), 3);
    }

    #[tokio::test]
    async fn multi_turn_retains_conversation_context() {
        // Two turns on the same loop; the conversation must accumulate.
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        let model = ScriptedModel::new(vec![
            ChatResponse {
                content: None,
                tool_calls: vec![tool_call("1", "final", r#"{"message":"done 1"}"#)],
            },
            ChatResponse {
                content: None,
                tool_calls: vec![tool_call("2", "final", r#"{"message":"done 2"}"#)],
            },
        ]);
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(model),
            runtime_with(docker),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        let t = CancellationToken::new();
        let r1 = agent.run_turn("first task", t.clone()).await.unwrap();
        let r2 = agent.run_turn("second task", t).await.unwrap();
        assert_eq!(r1.as_deref(), Some("done 1"));
        assert_eq!(r2.as_deref(), Some("done 2"));
        // Both user turns are retained in the conversation (context preserved).
        let users = agent
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .count();
        assert_eq!(users, 2);
        assert_eq!(agent.last_final.as_deref(), Some("done 2"));
    }

    #[tokio::test]
    async fn subagent_respects_max_depth() {
        let docker = MockDockerCli::new();
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            runtime_with(docker),
            cowboy_core::config::AgentBehavior::default(),
            200_000,
            CancellationToken::new(),
            &mut ui,
        );
        agent.subagent_depth = MAX_SUBAGENT_DEPTH; // already at the limit
        let result = agent
            .run_subagent(&super::super::tools::SubagentArgs {
                task: "do a thing".into(),
                context: None,
            })
            .await;
        // At max depth it returns an error string without spawning a subprocess.
        assert!(result.contains("maximum subagent depth"), "got: {result}");
    }

    #[tokio::test]
    async fn fit_context_prunes_old_history_keeping_system() {
        let docker = MockDockerCli::new();
        let mut ui = RecordingUi::default();
        let mut agent = AgentLoop::new(
            Box::new(ScriptedModel::new(vec![])),
            runtime_with(docker),
            cowboy_core::config::AgentBehavior::default(),
            RESPONSE_HEADROOM + 20, // tiny effective budget (~20 tokens)
            CancellationToken::new(),
            &mut ui,
        );
        for i in 0..40 {
            agent.messages.push(Message::user(format!(
                "message number {i} with several words here"
            )));
        }
        let before = agent.messages.len();
        agent.fit_context();
        assert!(agent.messages.len() < before, "should have pruned");
        assert_eq!(agent.messages[0].role, Role::System, "system kept");
        assert!(ui.notices.iter().any(|n| n.contains("pruned")));
    }

    #[test]
    fn truncate_keeps_short_output() {
        assert_eq!(truncate("hello", 100), "hello");
    }

    #[test]
    fn truncate_cuts_long_output_on_boundary() {
        let big = "x".repeat(1000);
        let t = truncate(&big, 100);
        assert!(t.starts_with(&"x".repeat(100)));
        assert!(t.contains("truncated"));
    }

    #[test]
    fn parse_args_handles_empty() {
        let a: FinalArgs = parse_args(r#"{"message":"done"}"#).unwrap();
        assert_eq!(a.message, "done");
    }
}
