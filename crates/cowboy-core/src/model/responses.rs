//! The OpenAI Responses API (`POST {base_url}/responses`): request bodies built
//! from Cowboy's [`Message`]s and [`ToolDef`]s.
//!
//! Cowboy keeps the whole conversation itself, so every request carries the full
//! history in `input` and sets `store: false` — nothing is kept server-side and
//! `previous_response_id` is never used. That is also the only shape ChatGPT plan
//! usage accepts, which is why one builder serves both: **strict** mode is the same
//! body minus every field that route refuses (sampling knobs, output caps).

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::{Message, Role, ToolDef};
use crate::config::ReasoningEffort;
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

/// How function tools are presented. Kept behind one switch because the
/// ChatGPT-plan route documents namespaced / `additional_tools` tools while every
/// other Responses endpoint takes them top-level.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolLayout {
    /// `tools: [{type: "function", name, description, parameters}]`.
    #[default]
    TopLevel,
}

/// The JSON body for one Responses request (`stream: true`).
pub fn build_body(opts: &RequestOptions, messages: &[Message], tools: &[ToolDef]) -> Result<Value> {
    let (instructions, input) = to_input(messages)?;
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
fn to_input(messages: &[Message]) -> Result<(Option<String>, Vec<Value>)> {
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
                    input.push(json!({
                        "type": "function_call",
                        "call_id": tc.id,
                        "name": name,
                        "arguments": arguments,
                    }));
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
    match layout {
        ToolLayout::TopLevel => Value::Array(
            tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    })
                })
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ToolCall;

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
    fn a_tool_result_without_an_id_is_refused() {
        let mut m = Message::tool_result("x", "out");
        m.tool_call_id = None;
        assert!(build_body(&opts(false), &[m], &[]).is_err());
    }
}
