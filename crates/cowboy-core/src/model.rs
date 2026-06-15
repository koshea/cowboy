//! OpenAI-compatible model client.
//!
//! A small domain model ([`Message`], [`ToolDef`], [`ToolCall`], [`ChatResponse`])
//! plus the [`ModelClient`] trait, and an [`OpenAiClient`] implementation built
//! on `async-openai`. The trait keeps the agent loop testable without a live
//! endpoint (tests provide a scripted fake).

use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestToolMessageArgs,
    ChatCompletionRequestUserMessageArgs, ChatCompletionTool, ChatCompletionTools,
    CreateChatCompletionRequestArgs, FunctionCall, FunctionObjectArgs,
};
use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use std::collections::BTreeMap;
use std::time::Duration;

use crate::config::{ReasoningEffort, ResolvedModel};
use crate::error::{Error, Result};

/// How many times to retry a rate-limited / transiently-failed chat request
/// before giving up.
const MAX_RETRIES: u32 = 5;

/// Retry a 429 (rate limit), 529 (overloaded), or any 5xx server error.
fn is_retryable_status(s: reqwest::StatusCode) -> bool {
    matches!(s.as_u16(), 429 | 529) || s.is_server_error()
}

/// Retry transient connection/timeout errors (not request-construction errors).
fn is_transient(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect()
}

/// Exponential backoff for retry `attempt` (0-based): 0.5s, 1s, 2s, … capped 16s.
fn backoff(attempt: u32) -> Duration {
    let secs = (0.5 * 2f64.powi(attempt as i32)).min(16.0);
    Duration::from_millis((secs * 1000.0) as u64)
}

/// Honor a numeric `Retry-After` header (seconds), capped at 60s.
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let secs: u64 = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs.min(60)))
}

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

/// A streamed piece of a model response: visible answer text, or the model's
/// "thinking" (reasoning) which is shown but never folded into the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delta {
    Content(String),
    Reasoning(String),
}

/// An OpenAI-compatible chat client. Streaming, cancellable (drop the future),
/// works against any OpenAI-compatible backend via a custom base URL.
#[async_trait]
pub trait ModelClient: Send + Sync {
    /// Run one chat turn. Streamed [`Delta`]s (answer text + reasoning) are sent
    /// to `deltas` (if any); the assembled response (content + tool calls) is
    /// returned. Reasoning is not part of the returned content.
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        deltas: Option<UnboundedSender<Delta>>,
    ) -> Result<ChatResponse>;
}

/// OpenAI-compatible client. Requests are built with `async-openai`'s typed
/// args, but streaming is done over a hand-rolled SSE parse so provider
/// `reasoning_content` (dropped by the typed stream) reaches the UI.
pub struct OpenAiClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    temperature: f32,
    max_tokens: u32,
    reasoning_effort: Option<ReasoningEffort>,
    top_p: Option<f32>,
    stop: Vec<String>,
    extra: BTreeMap<String, serde_json::Value>,
}

impl OpenAiClient {
    /// Build from a fully-resolved model (provider credentials joined with the
    /// model definition by [`crate::config::resolve_model`]).
    pub fn from_resolved(model: &ResolvedModel) -> Result<Self> {
        // Forward custom headers (provider defaults + per-model overrides).
        let mut headers = reqwest::header::HeaderMap::new();
        for (k, v) in &model.headers {
            if let (Ok(name), Ok(val)) = (
                reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                reqwest::header::HeaderValue::from_str(v),
            ) {
                headers.insert(name, val);
            }
        }
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .map_err(oa_err)?;

        Ok(Self {
            http,
            base_url: model.base_url.trim_end_matches('/').to_string(),
            api_key: model.api_key.clone(),
            model: model.model.clone(),
            temperature: model.temperature,
            max_tokens: model.max_tokens,
            reasoning_effort: model.reasoning_effort,
            top_p: model.top_p,
            stop: model.stop.clone(),
            extra: model.extra.clone(),
        })
    }
}

/// One model from a provider's `GET /models` catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    pub id: String,
    pub owned_by: String,
}

#[derive(Deserialize)]
struct ModelsList {
    #[serde(default)]
    data: Vec<ModelEntryWire>,
}
#[derive(Deserialize)]
struct ModelEntryWire {
    id: String,
    #[serde(default)]
    owned_by: String,
}

/// List a provider's available models via `GET {base_url}/models`. Reused by the
/// `/models` picker and `cowboy models available`.
pub async fn list_models(
    base_url: &str,
    api_key: &str,
    headers: &BTreeMap<String, String>,
) -> Result<Vec<ModelEntry>> {
    let mut hmap = reqwest::header::HeaderMap::new();
    for (k, v) in headers {
        if let (Ok(name), Ok(val)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) {
            hmap.insert(name, val);
        }
    }
    let http = reqwest::Client::builder()
        .default_headers(hmap)
        .build()
        .map_err(oa_err)?;
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let resp = http
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(oa_err)?;
    if !resp.status().is_success() {
        let code = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(Error::Model(format!(
            "listing models failed ({code}): {text}"
        )));
    }
    let list: ModelsList = resp.json().await.map_err(oa_err)?;
    Ok(list
        .data
        .into_iter()
        .map(|e| ModelEntry {
            id: e.id,
            owned_by: e.owned_by,
        })
        .collect())
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

/// Minimal view of an OpenAI-compatible streaming chunk that also captures
/// provider `reasoning_content` (aliased `reasoning`), which the typed
/// `async-openai` delta discards.
#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
}
#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
}
#[derive(Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default, alias = "reasoning")]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolChunk>>,
}
#[derive(Deserialize)]
struct ToolChunk {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FnChunk>,
}
#[derive(Deserialize)]
struct FnChunk {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[async_trait]
impl ModelClient for OpenAiClient {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        deltas: Option<UnboundedSender<Delta>>,
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
        let mut body = serde_json::to_value(&request).map_err(|e| Error::Model(e.to_string()))?;
        body["stream"] = serde_json::Value::Bool(true);
        if let Some(effort) = self.reasoning_effort {
            body["reasoning_effort"] = serde_json::Value::String(effort.as_str().into());
        }
        if let Some(top_p) = self.top_p {
            body["top_p"] = serde_json::json!(top_p);
        }
        if !self.stop.is_empty() {
            body["stop"] = serde_json::json!(self.stop);
        }
        // Config escape hatch: merge arbitrary top-level params last.
        if let Some(obj) = body.as_object_mut() {
            for (k, v) in &self.extra {
                obj.insert(k.clone(), v.clone());
            }
        }

        // Send with backoff-retry on rate limits (429), overload (529), 5xx, and
        // transient connection/timeout errors. Streaming hasn't started yet, so a
        // retry is clean (no partial output to discard).
        let url = format!("{}/chat/completions", self.base_url);
        let mut attempt = 0u32;
        let resp = loop {
            match self
                .http
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => break r,
                Ok(r) if is_retryable_status(r.status()) && attempt < MAX_RETRIES => {
                    let delay = retry_after(&r).unwrap_or_else(|| backoff(attempt));
                    tracing::warn!(
                        status = %r.status(),
                        attempt = attempt + 1,
                        delay_ms = delay.as_millis() as u64,
                        "model request throttled/failed; backing off and retrying"
                    );
                    attempt += 1;
                    tokio::time::sleep(delay).await;
                }
                Ok(r) => {
                    let code = r.status();
                    let text = r.text().await.unwrap_or_default();
                    return Err(Error::Model(format!(
                        "chat request failed ({code}): {text}"
                    )));
                }
                Err(e) if is_transient(&e) && attempt < MAX_RETRIES => {
                    let delay = backoff(attempt);
                    tracing::warn!(
                        error = %e,
                        attempt = attempt + 1,
                        delay_ms = delay.as_millis() as u64,
                        "model request connection error; backing off and retrying"
                    );
                    attempt += 1;
                    tokio::time::sleep(delay).await;
                }
                Err(e) => return Err(oa_err(e)),
            }
        };

        let mut content = String::new();
        let mut acc = ToolCallAccumulator::default();
        let mut buf = String::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk.map_err(oa_err)?));
            // SSE: process complete `\n`-terminated lines, leaving any partial
            // line in the buffer for the next chunk.
            while let Some(nl) = buf.find('\n') {
                let line: String = buf.drain(..=nl).collect();
                let Some(data) = line.trim().strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() {
                    continue;
                }
                if data == "[DONE]" {
                    return Ok(ChatResponse {
                        content: (!content.is_empty()).then_some(content),
                        tool_calls: acc.finish(),
                    });
                }
                let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) else {
                    continue; // skip keep-alives / unparseable frames
                };
                let Some(choice) = chunk.choices.into_iter().next() else {
                    continue;
                };
                if let Some(r) = choice.delta.reasoning_content.filter(|r| !r.is_empty()) {
                    if let Some(tx) = &deltas {
                        let _ = tx.send(Delta::Reasoning(r));
                    }
                }
                if let Some(text) = choice.delta.content.filter(|t| !t.is_empty()) {
                    if let Some(tx) = &deltas {
                        let _ = tx.send(Delta::Content(text.clone()));
                    }
                    content.push_str(&text);
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
