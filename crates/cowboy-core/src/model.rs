//! OpenAI-compatible model client.
//!
//! A small domain model ([`Message`], [`ToolDef`], [`ToolCall`], [`ChatResponse`])
//! plus the [`ModelClient`] trait, and an [`OpenAiClient`] implementation built
//! on `async-openai`. The trait keeps the agent loop testable without a live
//! endpoint (tests provide a scripted fake).

use async_openai::config::OpenAIConfig;
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestToolMessageArgs,
    ChatCompletionRequestUserMessageArgs, ChatCompletionTool, ChatCompletionTools,
    CreateChatCompletionRequestArgs, FunctionCall, FunctionObjectArgs,
};
use async_openai::Client;
use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::config::ModelProfile;
use crate::error::{Error, Result};

/// Role of a conversation message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A conversation message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: String,
    /// For `Tool` messages: the id of the tool call being answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// For `Assistant` messages: tool calls the model requested.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }
    /// A tool-result message answering `tool_call_id`.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: Vec::new(),
        }
    }
}

/// A tool the model may call (function-calling).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: serde_json::Value,
}

/// A tool call requested by the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON arguments string.
    pub arguments: String,
}

/// The model's response for one turn.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

/// An OpenAI-compatible chat client. Streaming, cancellable (drop the future),
/// works against any OpenAI-compatible backend via a custom base URL.
#[async_trait]
pub trait ModelClient: Send + Sync {
    /// Run one chat turn. Streamed text deltas are sent to `deltas` (if any);
    /// the assembled response (content + tool calls) is returned.
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        deltas: Option<UnboundedSender<String>>,
    ) -> Result<ChatResponse>;
}

/// `async-openai`-backed implementation.
pub struct OpenAiClient {
    client: Client<OpenAIConfig>,
    model: String,
    temperature: f32,
    max_tokens: u32,
}

impl OpenAiClient {
    /// Build from a model profile. The API key is read from the env var named
    /// by `profile.api_key_env` (never stored in config).
    pub fn from_profile(profile: &ModelProfile) -> Result<Self> {
        let api_key = std::env::var(&profile.api_key_env).unwrap_or_default();
        // NOTE: per-profile custom headers are accepted in config but not yet
        // forwarded (async-openai applies auth headers itself); follow-up.
        let config = OpenAIConfig::new()
            .with_api_base(profile.base_url.clone())
            .with_api_key(api_key);
        Ok(Self {
            client: Client::with_config(config),
            model: profile.model.clone(),
            temperature: profile.temperature,
            max_tokens: profile.max_tokens,
        })
    }
}

fn to_openai_messages(messages: &[Message]) -> Result<Vec<ChatCompletionRequestMessage>> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        let msg: ChatCompletionRequestMessage = match m.role {
            Role::System => ChatCompletionRequestSystemMessageArgs::default()
                .content(m.content.clone())
                .build()
                .map_err(oa_err)?
                .into(),
            Role::User => ChatCompletionRequestUserMessageArgs::default()
                .content(m.content.clone())
                .build()
                .map_err(oa_err)?
                .into(),
            Role::Assistant => {
                let mut args = ChatCompletionRequestAssistantMessageArgs::default();
                if !m.content.is_empty() {
                    args.content(m.content.clone());
                }
                if !m.tool_calls.is_empty() {
                    let calls: Vec<ChatCompletionMessageToolCalls> = m
                        .tool_calls
                        .iter()
                        .map(|tc| {
                            ChatCompletionMessageToolCalls::Function(
                                ChatCompletionMessageToolCall {
                                    id: tc.id.clone(),
                                    function: FunctionCall {
                                        name: tc.name.clone(),
                                        arguments: tc.arguments.clone(),
                                    },
                                },
                            )
                        })
                        .collect();
                    args.tool_calls(calls);
                }
                args.build().map_err(oa_err)?.into()
            }
            Role::Tool => ChatCompletionRequestToolMessageArgs::default()
                .content(m.content.clone())
                .tool_call_id(m.tool_call_id.clone().unwrap_or_default())
                .build()
                .map_err(oa_err)?
                .into(),
        };
        out.push(msg);
    }
    Ok(out)
}

fn to_openai_tools(tools: &[ToolDef]) -> Result<Vec<ChatCompletionTools>> {
    tools
        .iter()
        .map(|t| {
            let function = FunctionObjectArgs::default()
                .name(t.name.clone())
                .description(t.description.clone())
                .parameters(t.parameters.clone())
                .build()
                .map_err(oa_err)?;
            Ok(ChatCompletionTools::Function(ChatCompletionTool {
                function,
            }))
        })
        .collect()
}

fn oa_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Model(e.to_string())
}

/// Accumulates streamed tool-call chunks by index into complete [`ToolCall`]s.
#[derive(Default)]
struct ToolCallAccumulator {
    slots: Vec<(String, String, String)>, // (id, name, arguments)
}

impl ToolCallAccumulator {
    fn apply(
        &mut self,
        index: usize,
        id: Option<String>,
        name: Option<String>,
        args: Option<String>,
    ) {
        if self.slots.len() <= index {
            self.slots
                .resize(index + 1, (String::new(), String::new(), String::new()));
        }
        let slot = &mut self.slots[index];
        if let Some(id) = id {
            if !id.is_empty() {
                slot.0 = id;
            }
        }
        if let Some(name) = name {
            if !name.is_empty() {
                slot.1 = name;
            }
        }
        if let Some(args) = args {
            slot.2.push_str(&args);
        }
    }

    fn finish(self) -> Vec<ToolCall> {
        self.slots
            .into_iter()
            .filter(|(_, name, _)| !name.is_empty())
            .map(|(id, name, arguments)| ToolCall {
                id,
                name,
                arguments,
            })
            .collect()
    }
}

#[async_trait]
impl ModelClient for OpenAiClient {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        deltas: Option<UnboundedSender<String>>,
    ) -> Result<ChatResponse> {
        let mut builder = CreateChatCompletionRequestArgs::default();
        builder
            .model(&self.model)
            .temperature(self.temperature)
            .max_tokens(self.max_tokens)
            .messages(to_openai_messages(messages)?);
        if !tools.is_empty() {
            builder.tools(to_openai_tools(tools)?);
        }
        let request = builder.build().map_err(oa_err)?;

        let mut stream = self
            .client
            .chat()
            .create_stream(request)
            .await
            .map_err(oa_err)?;

        let mut content = String::new();
        let mut acc = ToolCallAccumulator::default();
        while let Some(item) = stream.next().await {
            let response = item.map_err(oa_err)?;
            let Some(choice) = response.choices.into_iter().next() else {
                continue;
            };
            if let Some(text) = choice.delta.content {
                if !text.is_empty() {
                    if let Some(tx) = &deltas {
                        let _ = tx.send(text.clone());
                    }
                    content.push_str(&text);
                }
            }
            if let Some(calls) = choice.delta.tool_calls {
                for tc in calls {
                    let (name, args) = match tc.function {
                        Some(f) => (f.name, f.arguments),
                        None => (None, None),
                    };
                    acc.apply(tc.index as usize, tc.id, name, args);
                }
            }
        }

        Ok(ChatResponse {
            content: (!content.is_empty()).then_some(content),
            tool_calls: acc.finish(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_accumulator_assembles_streamed_chunks() {
        let mut acc = ToolCallAccumulator::default();
        acc.apply(
            0,
            Some("call_1".into()),
            Some("shell".into()),
            Some("{\"comm".into()),
        );
        acc.apply(0, None, None, Some("and\":\"ls\"}".into()));
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments, "{\"command\":\"ls\"}");
    }

    #[test]
    fn accumulator_drops_empty_nameless_slots() {
        let mut acc = ToolCallAccumulator::default();
        acc.apply(0, None, None, Some("orphan".into()));
        assert!(acc.finish().is_empty());
    }

    #[test]
    fn message_helpers() {
        assert_eq!(Message::user("hi").role, Role::User);
        let t = Message::tool_result("id1", "out");
        assert_eq!(t.role, Role::Tool);
        assert_eq!(t.tool_call_id.as_deref(), Some("id1"));
    }
}
