//! The OpenAI Responses API (`POST {base_url}/responses`): request bodies built
//! from Cowboy's [`Message`]s and [`ToolDef`]s.
//!
//! Cowboy keeps the whole conversation itself, so every request carries the full
//! history in `input` and sets `store: false` — nothing is kept server-side and
//! `previous_response_id` is never used. That is also the only shape ChatGPT plan
//! usage accepts, which is why one builder serves both: **strict** mode is the same
//! body minus every field that route refuses (sampling knobs, output caps).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

use super::transport::{self, FailurePolicy, SseStream, TokenSource};
use super::{
    header_map, oa_err, ChatResponse, Delta, Message, ModelClient, ReasoningItems, Role, ToolCall,
    ToolDef, Usage,
};
use crate::config::{ReasoningEffort, ResolvedModel};
use crate::error::{Error, Result};

/// Everything about a request that comes from the model's configuration rather
/// than from the conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestOptions {
    pub model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub top_p: Option<f32>,
    /// Arbitrary top-level params merged in last (the config escape hatch).
    pub extra: BTreeMap<String, Value>,
    /// Omit every field ChatGPT plan usage rejects (see [`STRICT_OMITTED`]).
    pub strict: bool,
    pub tool_layout: ToolLayout,
    /// Ask for (and replay) encrypted reasoning items. Only reasoning models
    /// produce them, and some endpoints reject the `include` for other models, so
    /// this is on when a reasoning effort is configured or in strict mode (every
    /// ChatGPT-plan model reasons).
    pub include_reasoning: bool,
    /// Which endpoint + model produced reasoning items; only matching items are
    /// replayed (see [`ReasoningItems`]).
    pub origin: String,
}

/// Fields the ChatGPT-plan route refuses, per OpenAI's preview limitations. The
/// builder never emits most of them; the ones it would (`temperature`, `top_p`,
/// `max_output_tokens`) are left out in strict mode, and all of them are stripped
/// from `extra` too, so a config escape hatch cannot break every request.
pub const STRICT_OMITTED: &[&str] = &[
    "background",
    "conversation",
    "max_output_tokens",
    "max_tool_calls",
    "metadata",
    "moderation",
    "multi_agent",
    "previous_response_id",
    "prompt",
    "prompt_cache_retention",
    "safety_identifier",
    "temperature",
    "top_logprobs",
    "top_p",
    "truncation",
    "user",
];

/// How function tools are presented. The ChatGPT-plan route documents function
/// tools grouped in namespaces (or `additional_tools`), while plain Responses
/// endpoints — and gateways translating to other providers, which may drop what
/// is not `type: function` — take them top-level.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolLayout {
    /// `tools: [{type: "function", name, description, parameters}]`.
    #[default]
    TopLevel,
    /// `tools: [{type: "namespace", name, description, tools: [functions]}]`. Calls
    /// come back with a bare `name` plus `namespace`, and are replayed the same way.
    Namespace(&'static str),
}

/// The namespace Cowboy's tools are grouped under on the ChatGPT-plan route.
pub const TOOL_NAMESPACE: &str = "cowboy";

/// The JSON body for one Responses request (`stream: true`).
pub fn build_body(opts: &RequestOptions, messages: &[Message], tools: &[ToolDef]) -> Result<Value> {
    let replay = opts.include_reasoning.then_some(opts.origin.as_str());
    let (instructions, input) = to_input(messages, replay, opts.tool_layout)?;
    let mut body = json!({
        "model": opts.model,
        "input": input,
        "stream": true,
        "store": false,
    });
    if let Some(i) = instructions {
        body["instructions"] = Value::String(i);
    }
    if !tools.is_empty() {
        body["tools"] = to_tools(tools, opts.tool_layout);
    }
    if let Some(effort) = opts.reasoning_effort {
        body["reasoning"] = json!({ "effort": effort.as_str() });
    }
    if opts.include_reasoning {
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    if !opts.strict {
        body["temperature"] = short_float(opts.temperature);
        // Caps reasoning + answer together, like `max_completion_tokens`.
        body["max_output_tokens"] = json!(opts.max_tokens);
        if let Some(top_p) = opts.top_p {
            body["top_p"] = short_float(top_p);
        }
    }
    if let Some(obj) = body.as_object_mut() {
        for (k, v) in &opts.extra {
            if opts.strict && STRICT_OMITTED.contains(&k.as_str()) {
                continue;
            }
            obj.insert(k.clone(), v.clone());
        }
    }
    Ok(body)
}

/// Split the conversation into `instructions` and `input` items.
///
/// The first system message is the system prompt and becomes `instructions`.
/// Later system messages (notes Cowboy inserts mid-conversation) become
/// `developer` messages in place: a `system` item is refused by the ChatGPT-plan
/// route, and folding them into `instructions` would move them out of order.
///
/// With `replay` set to this request's origin, an assistant turn's reasoning items
/// from that same origin are replayed before its output — the order the model
/// produced them in.
fn to_input(
    messages: &[Message],
    replay: Option<&str>,
    layout: ToolLayout,
) -> Result<(Option<String>, Vec<Value>)> {
    let mut instructions: Option<String> = None;
    let mut input = Vec::with_capacity(messages.len());
    for m in messages {
        match m.role {
            Role::System if instructions.is_none() && input.is_empty() => {
                instructions = Some(m.content.clone());
            }
            Role::System => input.push(message("developer", &m.content)),
            Role::User => input.push(message("user", &m.content)),
            Role::Assistant => {
                if let (Some(origin), Some(r)) = (replay, &m.reasoning_items) {
                    if r.origin == origin {
                        input.extend(r.items.iter().cloned());
                    }
                }
                if !m.content.is_empty() {
                    input.push(message("assistant", &m.content));
                }
                for tc in &m.tool_calls {
                    // The same normalization as the Chat Completions path: a blank
                    // name or arguments echoed back makes a strict endpoint reject
                    // the whole request, so one bad call would brick the session.
                    let arguments = if tc.arguments.trim().is_empty() {
                        "{}".to_string()
                    } else {
                        tc.arguments.clone()
                    };
                    let name = if tc.name.trim().is_empty() {
                        "unknown".to_string()
                    } else {
                        tc.name.clone()
                    };
                    let mut call = json!({
                        "type": "function_call",
                        "call_id": tc.id,
                        "name": name,
                        "arguments": arguments,
                    });
                    if let ToolLayout::Namespace(ns) = layout {
                        call["namespace"] = json!(ns);
                    }
                    input.push(call);
                }
            }
            Role::Tool => {
                let id = m
                    .tool_call_id
                    .clone()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        Error::Model(
                            "a tool result has no tool_call_id, so it cannot be matched to \
                             its call; refusing to send an unanswerable conversation"
                                .to_string(),
                        )
                    })?;
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": id,
                    "output": m.content,
                }));
            }
        }
    }
    Ok((instructions, input))
}

/// An `f32` sampling knob as the decimal the user wrote (`0.2`, not
/// `0.20000000298023224`): config values are short decimals, and the long form
/// only makes request logs harder to read.
fn short_float(x: f32) -> Value {
    json!((f64::from(x) * 1e6).round() / 1e6)
}

fn message(role: &str, content: &str) -> Value {
    json!({ "type": "message", "role": role, "content": content })
}

fn to_tools(tools: &[ToolDef], layout: ToolLayout) -> Value {
    let functions: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            })
        })
        .collect();
    match layout {
        ToolLayout::TopLevel => Value::Array(functions),
        ToolLayout::Namespace(ns) => json!([{
            "type": "namespace",
            "name": ns,
            "description": "Cowboy's tools: the workspace, the shell, and session coordination.",
            "tools": functions,
        }]),
    }
}

/// A streaming Responses API client. Speaks to any endpoint that implements
/// `POST /responses` — OpenAI, or a gateway such as Bifrost — and, in strict
/// mode with an OAuth token source, to ChatGPT plan usage.
#[derive(Clone)]
pub struct ResponsesClient {
    http: reqwest::Client,
    url: String,
    auth: Arc<dyn TokenSource>,
    failures: Arc<dyn FailurePolicy>,
    opts: RequestOptions,
    stream_idle: Duration,
}

impl ResponsesClient {
    /// Build from a resolved model with the given token source and failure
    /// policy. `strict` selects the ChatGPT-plan body (see [`STRICT_OMITTED`]).
    pub fn new(
        model: &ResolvedModel,
        auth: Arc<dyn TokenSource>,
        failures: Arc<dyn FailurePolicy>,
        strict: bool,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .default_headers(header_map(&model.headers))
            .build()
            .map_err(oa_err)?;
        Ok(Self {
            http,
            url: format!("{}/responses", model.base_url.trim_end_matches('/')),
            auth,
            failures,
            opts: RequestOptions {
                model: model.model.clone(),
                temperature: model.temperature,
                max_tokens: model.max_tokens,
                reasoning_effort: model.reasoning_effort,
                top_p: model.top_p,
                extra: model.extra.clone(),
                strict,
                tool_layout: if strict {
                    ToolLayout::Namespace(TOOL_NAMESPACE)
                } else {
                    ToolLayout::TopLevel
                },
                include_reasoning: strict || model.reasoning_effort.is_some(),
                origin: format!("{}#{}", model.base_url.trim_end_matches('/'), model.model),
            },
            stream_idle: model
                .stream_idle_timeout_seconds
                .map(Duration::from_secs)
                .unwrap_or(super::DEFAULT_STREAM_IDLE),
        })
    }
}

/// One streamed event. Only the fields Cowboy reads; everything else is ignored
/// so a provider adding event types or fields never breaks the stream.
#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    output_index: Option<u64>,
    #[serde(default)]
    item: Option<Item>,
    #[serde(default)]
    arguments: Option<String>,
    #[serde(default)]
    response: Option<ResponseObj>,
}

#[derive(Deserialize, Default)]
struct Item {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize, Default)]
struct ResponseObj {
    #[serde(default)]
    usage: Option<UsageWire>,
    #[serde(default)]
    incomplete_details: Option<IncompleteWire>,
}

#[derive(Deserialize)]
struct UsageWire {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<InputDetails>,
}

#[derive(Deserialize)]
struct InputDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Deserialize)]
struct IncompleteWire {
    #[serde(default)]
    reason: Option<String>,
}

/// A failure the provider reported inside the stream (`response.failed` or an
/// `error` event), with its machine-readable code kept for the caller.
pub fn stream_error(code: Option<&str>, message: Option<&str>, param: Option<&str>) -> Error {
    let code_s = code.unwrap_or("unknown_error");
    let mut msg = format!(
        "model response failed ({code_s}): {}",
        message.unwrap_or("no message")
    );
    if let Some(p) = param {
        msg.push_str(&format!(" [param: {p}]"));
    }
    if matches!(code, Some("model_not_found")) {
        Error::ModelUnavailable(msg)
    } else {
        Error::Model(msg)
    }
}

/// Function calls assembled by output index (the stream's own ordering).
#[derive(Default)]
struct Calls(BTreeMap<u64, (String, String, String)>);

impl Calls {
    fn slot(&mut self, index: u64) -> &mut (String, String, String) {
        self.0.entry(index).or_default()
    }

    /// The finished calls; one with no id gets `call_{index}` so its result can
    /// still be addressed (see the Chat Completions accumulator for why).
    fn finish(self) -> Vec<ToolCall> {
        self.0
            .into_iter()
            .filter(|(_, (_, name, _))| !name.is_empty())
            .map(|(i, (id, name, arguments))| ToolCall {
                id: if id.is_empty() {
                    format!("call_{i}")
                } else {
                    id
                },
                name,
                arguments,
            })
            .collect()
    }
}

#[async_trait]
impl ModelClient for ResponsesClient {
    fn max_output_tokens(&self) -> usize {
        self.opts.max_tokens as usize
    }

    /// Same endpoint and budget, `reasoning.effort: minimal` — see the trait.
    fn with_minimal_reasoning(&self) -> Option<Box<dyn ModelClient>> {
        if self.opts.reasoning_effort == Some(ReasoningEffort::Minimal) {
            return None;
        }
        let mut c = self.clone();
        c.opts.reasoning_effort = Some(ReasoningEffort::Minimal);
        Some(Box::new(c))
    }

    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        deltas: Option<UnboundedSender<Delta>>,
    ) -> Result<ChatResponse> {
        let body = build_body(&self.opts, messages, tools)?;
        let resp = transport::send_with_retry(
            &self.http,
            &self.url,
            &body,
            self.auth.as_ref(),
            self.failures.as_ref(),
        )
        .await?;
        let rid = transport::request_id(&resp);
        let mut sse = SseStream::new(resp, self.stream_idle);
        read_stream(
            &mut sse,
            deltas.as_ref(),
            &self.opts.origin,
            self.failures.as_ref(),
        )
        .await
        .map_err(|e| with_request_id(e, rid.as_deref()))
    }
}

fn with_request_id(e: Error, rid: Option<&str>) -> Error {
    let Some(rid) = rid else { return e };
    match e {
        Error::Model(m) => Error::Model(format!("{m} [request {rid}]")),
        Error::ModelUnavailable(m) => Error::ModelUnavailable(format!("{m} [request {rid}]")),
        other => other,
    }
}

/// Fold a Responses event stream into one [`ChatResponse`]. Success is only a
/// `response.completed` (or `response.incomplete`, which is a truncation); a
/// stream that simply ends is an interrupted response, not an empty answer.
async fn read_stream(
    sse: &mut SseStream,
    deltas: Option<&UnboundedSender<Delta>>,
    origin: &str,
    failures: &dyn FailurePolicy,
) -> Result<ChatResponse> {
    let mut reasoning_items: Vec<Value> = Vec::new();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut calls = Calls::default();
    let send = |d: Delta| {
        if let Some(tx) = deltas {
            let _ = tx.send(d);
        }
    };
    while let Some(data) = sse.next_data().await? {
        if data == "[DONE]" {
            break;
        }
        let Ok(raw) = serde_json::from_str::<Value>(&data) else {
            continue; // keep-alives / unparseable frames
        };
        // A finished reasoning item is kept verbatim to replay next turn.
        if raw["type"] == "response.output_item.done" && raw["item"]["type"] == "reasoning" {
            reasoning_items.push(raw["item"].clone());
            continue;
        }
        // A failure keeps the event itself: providers differ in where they put the
        // code (top level, or nested under `error`), and a failure whose reason
        // cannot be found must still show what was sent rather than "no message".
        if raw["type"] == "response.failed" || raw["type"] == "error" {
            return Err(failed_event(&raw, failures));
        }
        let Ok(ev) = serde_json::from_value::<Event>(raw) else {
            continue;
        };
        match ev.kind.as_str() {
            "response.output_text.delta" => {
                if let Some(t) = ev.delta.filter(|t| !t.is_empty()) {
                    send(Delta::Content(t.clone()));
                    content.push_str(&t);
                }
            }
            // A reasoning summary (OpenAI) or raw reasoning text (some gateways and
            // open models): shown as thinking, never folded into the answer.
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(t) = ev.delta.filter(|t| !t.is_empty()) {
                    send(Delta::Reasoning(t.clone()));
                    reasoning.push_str(&t);
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                let (Some(i), Some(item)) = (ev.output_index, ev.item) else {
                    continue;
                };
                if item.kind != "function_call" {
                    continue;
                }
                let done = ev.kind.ends_with(".done");
                let slot = calls.slot(i);
                if let Some(id) = item.call_id.filter(|s| !s.is_empty()) {
                    slot.0 = id;
                }
                if let Some(n) = item.name.filter(|s| !s.is_empty()) {
                    slot.1 = n;
                }
                // `.done` carries the authoritative full arguments.
                match item.arguments {
                    Some(a) if done && !a.is_empty() => slot.2 = a,
                    Some(a) if !done && slot.2.is_empty() => slot.2 = a,
                    _ => {}
                }
            }
            "response.function_call_arguments.delta" => {
                if let (Some(i), Some(d)) = (ev.output_index, ev.delta) {
                    calls.slot(i).2.push_str(&d);
                }
            }
            "response.function_call_arguments.done" => {
                if let (Some(i), Some(a)) = (ev.output_index, ev.arguments) {
                    calls.slot(i).2 = a;
                }
            }
            "response.completed" | "response.incomplete" => {
                let r = ev.response.unwrap_or_default();
                let truncated = ev.kind == "response.incomplete"
                    && r.incomplete_details
                        .as_ref()
                        .and_then(|d| d.reason.as_deref())
                        .is_none_or(|reason| reason == "max_output_tokens");
                return Ok(ChatResponse {
                    content: (!content.is_empty()).then_some(content),
                    tool_calls: calls.finish(),
                    reasoning: (!reasoning.is_empty()).then_some(reasoning),
                    truncated,
                    reasoning_items: (!reasoning_items.is_empty()).then(|| ReasoningItems {
                        origin: origin.to_string(),
                        items: reasoning_items,
                    }),
                    usage: r.usage.map(|u| Usage {
                        prompt_tokens: u.input_tokens,
                        completion_tokens: u.output_tokens,
                        cached_prompt_tokens: u
                            .input_tokens_details
                            .map(|d| d.cached_tokens)
                            .unwrap_or(0),
                    }),
                });
            }
            _ => {}
        }
    }
    Err(Error::Model(
        "model stream ended before the response completed (no `response.completed`); \
         the answer may be cut off — retry the turn"
            .into(),
    ))
}

/// The error for a `response.failed` or `error` event. The reason is looked for
/// where the Responses API documents it (`response.error`, or the event's own
/// `code`/`message`) and where providers also put it (`error: {…}`,
/// `response.status_details.error`); if none is there, the event is quoted.
fn failed_event(raw: &Value, failures: &dyn FailurePolicy) -> Error {
    tracing::debug!(event = %quote(raw), "model response failed");
    let candidates = [
        &raw["response"]["error"],
        &raw["error"],
        raw,
        &raw["response"]["status_details"]["error"],
    ];
    let str_of = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    for c in candidates {
        let (code, message) = (
            str_of(c, "code").or_else(|| {
                str_of(c, "type").filter(|t| t != "error" && !t.starts_with("response."))
            }),
            str_of(c, "message"),
        );
        if message.is_some() {
            return failures.stream_error(
                code.as_deref(),
                message.as_deref(),
                str_of(c, "param").as_deref(),
            );
        }
        if let Some(code) = code {
            // A code alone says little when it is generic; keep what came with it.
            let e = failures.stream_error(Some(&code), None, str_of(c, "param").as_deref());
            return match e {
                Error::Model(m) => Error::Model(format!("{m}; the provider sent: {}", quote(raw))),
                other => other,
            };
        }
    }
    Error::Model(format!(
        "model response failed without a reason; the provider sent: {}",
        quote(raw)
    ))
}

/// An event for an error message, bounded.
fn quote(raw: &Value) -> String {
    let q = raw.to_string();
    match q.char_indices().nth(800) {
        Some((i, _)) => format!("{}…", &q[..i]),
        None => q,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(strict: bool) -> RequestOptions {
        RequestOptions {
            model: "gpt-x".into(),
            temperature: 0.2,
            max_tokens: 4096,
            reasoning_effort: None,
            top_p: None,
            extra: BTreeMap::new(),
            strict,
            tool_layout: ToolLayout::TopLevel,
            include_reasoning: false,
            origin: "o".into(),
        }
    }

    fn shell_tool() -> ToolDef {
        ToolDef {
            name: "shell".into(),
            description: "run a command".into(),
            parameters: json!({"type": "object", "properties": {"cmd": {"type": "string"}}}),
        }
    }

    fn pretty(v: &Value) -> String {
        serde_json::to_string_pretty(v).unwrap()
    }

    #[test]
    fn plain_turn() {
        let msgs = [Message::system("be terse"), Message::user("hi")];
        insta::assert_snapshot!(pretty(&build_body(&opts(false), &msgs, &[]).unwrap()));
    }

    #[test]
    fn tool_round_trip() {
        let mut assistant = Message::new(Role::Assistant, "looking");
        assistant.tool_calls = vec![
            ToolCall {
                id: "call_1".into(),
                name: "shell".into(),
                arguments: r#"{"cmd":"ls"}"#.into(),
            },
            // Malformed: normalized rather than echoed back.
            ToolCall {
                id: "call_2".into(),
                name: " ".into(),
                arguments: "".into(),
            },
        ];
        let msgs = [
            Message::system("sys"),
            Message::user("list files"),
            assistant,
            Message::tool_result("call_1", "a.rs\nb.rs"),
            Message::tool_result("call_2", "error: unknown tool"),
            Message::system("note: budget is low"),
        ];
        insta::assert_snapshot!(pretty(
            &build_body(&opts(false), &msgs, &[shell_tool()]).unwrap()
        ));
    }

    #[test]
    fn reasoning_effort_and_top_p() {
        let mut o = opts(false);
        o.reasoning_effort = Some(ReasoningEffort::High);
        o.top_p = Some(0.9);
        let body = build_body(&o, &[Message::user("think")], &[]).unwrap();
        assert_eq!(body["reasoning"], json!({"effort": "high"}));
        assert_eq!(body["top_p"], json!(0.9));
        assert!(body.get("instructions").is_none());
    }

    #[test]
    fn strict_mode_omits_every_refused_field() {
        let mut o = opts(true);
        o.tool_layout = ToolLayout::Namespace(TOOL_NAMESPACE);
        o.top_p = Some(0.9);
        o.reasoning_effort = Some(ReasoningEffort::Low);
        o.extra.insert("temperature".into(), json!(1.0));
        o.extra.insert("metadata".into(), json!({"a": 1}));
        o.extra.insert("parallel_tool_calls".into(), json!(true));
        let msgs = [Message::system("sys"), Message::user("hi")];
        let body = build_body(&o, &msgs, &[shell_tool()]).unwrap();
        for k in STRICT_OMITTED {
            assert!(body.get(*k).is_none(), "{k} must be omitted in strict mode");
        }
        assert_eq!(body["store"], json!(false));
        assert_eq!(body["stream"], json!(true));
        // Allowed extras still pass through.
        assert_eq!(body["parallel_tool_calls"], json!(true));
        // No `system` items anywhere.
        let input = body["input"].as_array().unwrap();
        assert!(input.iter().all(|i| i["role"] != "system"));
        insta::assert_snapshot!(pretty(&body));
    }

    #[test]
    fn extra_merges_last_in_normal_mode() {
        let mut o = opts(false);
        o.extra.insert("temperature".into(), json!(1.0));
        o.extra.insert("truncation".into(), json!("auto"));
        let body = build_body(&o, &[Message::user("x")], &[]).unwrap();
        assert_eq!(body["temperature"], json!(1.0));
        assert_eq!(body["truncation"], json!("auto"));
    }

    #[test]
    fn reasoning_items_are_replayed_only_to_the_model_that_wrote_them() {
        let item = json!({"type": "reasoning", "id": "rs_1", "summary": [],
                          "encrypted_content": "gAAA"});
        let mut assistant = Message::new(Role::Assistant, "");
        assistant.tool_calls = vec![ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: "{}".into(),
        }];
        assistant.reasoning_items = Some(ReasoningItems {
            origin: "o".into(),
            items: vec![item.clone()],
        });
        let msgs = [
            Message::user("go"),
            assistant,
            Message::tool_result("c1", "ok"),
        ];
        let mut o = opts(true);
        o.include_reasoning = true;
        let body = build_body(&o, &msgs, &[]).unwrap();
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        let input = body["input"].as_array().unwrap();
        // Replayed before the call it led to.
        assert_eq!(input[1], item);
        assert_eq!(input[2]["type"], "function_call");
        insta::assert_snapshot!(pretty(&body));

        // Another model's items are dropped, not sent where they would fail.
        o.origin = "elsewhere".into();
        let body = build_body(&o, &msgs, &[]).unwrap();
        assert!(body["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["type"] != "reasoning"));
        // And nothing is replayed or requested when reasoning is off.
        let body = build_body(&opts(false), &msgs, &[]).unwrap();
        assert!(body.get("include").is_none());
    }

    #[test]
    fn namespaced_tools_are_grouped_and_their_calls_replayed_with_the_namespace() {
        let mut o = opts(true);
        o.tool_layout = ToolLayout::Namespace(TOOL_NAMESPACE);
        let mut assistant = Message::new(Role::Assistant, "");
        assistant.tool_calls = vec![ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: "{}".into(),
        }];
        let msgs = [
            Message::user("go"),
            assistant,
            Message::tool_result("c1", "ok"),
        ];
        let body = build_body(&o, &msgs, &[shell_tool()]).unwrap();
        assert_eq!(body["tools"][0]["type"], "namespace");
        assert_eq!(body["tools"][0]["name"], TOOL_NAMESPACE);
        assert_eq!(body["tools"][0]["tools"][0]["name"], "shell");
        assert_eq!(body["input"][1]["namespace"], TOOL_NAMESPACE);
        // Top-level elsewhere: nothing namespaced leaks into a gateway request.
        let body = build_body(&opts(false), &msgs, &[shell_tool()]).unwrap();
        assert_eq!(body["tools"][0]["type"], "function");
        assert!(body["input"][1].get("namespace").is_none());
    }

    #[test]
    fn a_tool_result_without_an_id_is_refused() {
        let mut m = Message::tool_result("x", "out");
        m.tool_call_id = None;
        assert!(build_body(&opts(false), &[m], &[]).is_err());
    }
}
