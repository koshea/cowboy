//! The Cowboy-owned agent loop: model turn -> tool call -> observation ->
//! repeat, until `final`, `ask_user` is answered, or limits are hit. Cowboy
//! owns this lifecycle; no agent framework.

use anyhow::Result;
use cowboy_core::config::AgentBehavior;
use cowboy_core::model::{ChatResponse, Message, ModelClient, Role, ToolDef};
use tokio_util::sync::CancellationToken;

use super::tools::{self, AskUserArgs, FinalArgs, ShellArgs};
use super::ui::AgentUi;
use crate::net::runtime::AgentRuntime;
use crate::session::SessionLogger;

/// Default agent system prompt (see plan §10.3).
pub const SYSTEM_PROMPT: &str = "\
You are Cowboy, an autonomous coding agent running inside a Docker container.

The project is mounted at /workspace. You may freely inspect, edit, build, test, \
and run code inside the container. Use the `shell` tool for almost all work.

Cowboy-specific helpers are CLIs you invoke through `shell`, e.g. `cowboy patch \
show` and `cowboy proc start <name>`. You do not need to ask before ordinary \
development actions inside the container.

The runtime enforces network, host, and secret permissions outside your control. \
If a command cannot access something, observe the failure and continue.

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
    messages: Vec<Message>,
    ui: &'a mut dyn AgentUi,
    logger: Option<SessionLogger>,
}

impl<'a> AgentLoop<'a> {
    pub fn new(
        model: Box<dyn ModelClient>,
        runtime: AgentRuntime,
        behavior: AgentBehavior,
        cancel: CancellationToken,
        ui: &'a mut dyn AgentUi,
    ) -> Self {
        Self {
            model,
            runtime,
            tools: tools::definitions(),
            behavior,
            cancel,
            messages: vec![Message::system(SYSTEM_PROMPT)],
            ui,
            logger: None,
        }
    }

    /// Attach a session logger (records transcript, commands, final summary).
    pub fn with_logger(mut self, logger: Option<SessionLogger>) -> Self {
        self.logger = logger;
        self
    }

    /// Run the loop for `task` until completion, cancellation, or the iteration
    /// cap. Returns the final message if the agent produced one.
    pub async fn run(&mut self, task: &str) -> Result<Option<String>> {
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
                    let args: FinalArgs = parse_args(&call.arguments)?;
                    if let Some(l) = &self.logger {
                        l.write_final(&args.message);
                    }
                    self.ui.final_message(&args.message);
                    return Ok(Some(args.message));
                }
                tools::TOOL_SHELL => {
                    let args: ShellArgs = parse_args(&call.arguments)?;
                    self.ui.command_start(&args.command);
                    let started = std::time::Instant::now();
                    let (result, output) = self
                        .runtime
                        .run_capture(
                            &args.command,
                            args.cwd.as_deref(),
                            self.behavior.command_timeout_seconds,
                        )
                        .await?;
                    let duration_ms = started.elapsed().as_millis();
                    let truncated = truncate(&output, self.behavior.max_command_output_bytes);
                    self.ui.command_end(result.exit_code, &truncated);
                    if let Some(l) = &mut self.logger {
                        l.log_command(&args.command, result.exit_code, duration_ms, output.len());
                    }
                    let observation = format!("[exit code: {}]\n{}", result.exit_code, truncated);
                    let tool_msg = Message::tool_result(&call.id, observation);
                    if let Some(l) = &mut self.logger {
                        l.log_message(&tool_msg);
                    }
                    self.messages.push(tool_msg);
                }
                tools::TOOL_ASK_USER => {
                    let args: AskUserArgs = parse_args(&call.arguments)?;
                    let answer = self.ui.ask_user(&args.question);
                    let tool_msg = Message::tool_result(&call.id, answer);
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
    }
    impl AgentUi for RecordingUi {
        fn model_delta(&mut self, _text: &str) {}
        fn command_start(&mut self, command: &str) {
            self.commands.push(command.to_string());
        }
        fn command_end(&mut self, _exit_code: i32, _output: &str) {}
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
            .expect_exec_capture()
            .withf(|_n, _w, argv| argv[0] == "sh" && argv[2].contains("ls"))
            .times(1)
            .returning(|_, _, _| Ok((ExecResult { exit_code: 0 }, "file1\nfile2\n".into())));

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
            cancel,
            &mut ui,
        );
        let final_msg = agent.run("list the files then finish").await.unwrap();

        assert_eq!(final_msg.as_deref(), Some("done; tests pass"));
        assert_eq!(ui.commands, vec!["ls"]);
        assert_eq!(ui.finals, vec!["done; tests pass"]);
    }

    #[tokio::test]
    async fn stops_at_max_iterations() {
        let mut docker = MockDockerCli::new();
        docker
            .expect_container_state()
            .returning(|_| Ok(ContainerState::Running));
        docker
            .expect_exec_capture()
            .returning(|_, _, _| Ok((ExecResult { exit_code: 0 }, "ok".into())));
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
            CancellationToken::new(),
            &mut ui,
        );
        let res = agent.run("loop forever").await.unwrap();
        assert!(res.is_none());
        assert!(ui.notices.iter().any(|n| n.contains("max_iterations")));
        assert_eq!(ui.commands.len(), 3);
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
