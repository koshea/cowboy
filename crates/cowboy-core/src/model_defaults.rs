//! Curated, updatable defaults for known models.
//!
//! Provider catalogues (`GET /models`) return only ids — no context window,
//! sampling, or reasoning hints. This module supplies recommended settings and a
//! friendly name for known models so the `/models` picker and `cowboy models
//! add` can prefill sensible values.
//!
//! Sources, in priority order:
//! 1. `~/.config/cowboy/model-defaults.yaml` (user override / extension)
//! 2. the embedded table ([`model_defaults.yaml`])
//!
//! Entries match by exact `id` or by `prefix` (longest match wins; overrides win
//! ties), so future point-releases of a family inherit a sane baseline. A hosted
//! fetch can later layer in front of the override file.

use std::sync::OnceLock;

use serde::Deserialize;

use crate::config::{global_config_dir, ReasoningEffort};

const EMBEDDED: &str = include_str!("model_defaults.yaml");

/// Hard fallback when nothing matches.
const FALLBACK_CONTEXT: u32 = 131_072;
const FALLBACK_MAX_TOKENS: u32 = 8_192;
const FALLBACK_TEMPERATURE: f32 = 0.7;

#[derive(Debug, Clone, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    models: Vec<Entry>,
}

#[derive(Debug, Clone, Deserialize)]
struct Entry {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context_window: Option<u32>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    reasoning_effort: Option<ReasoningEffort>,
    #[serde(default = "default_true")]
    chat: bool,
}

fn default_true() -> bool {
    true
}

/// A resolved default suggestion for a model id.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelDefault {
    pub name: String,
    pub context_window: u32,
    pub max_tokens: u32,
    pub temperature: f32,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub chat: bool,
}

/// The merged registry: override entries first (they win ties), then embedded.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    entries: Vec<Entry>,
}

impl Registry {
    /// Parse one YAML document's entries (test seam).
    fn parse(yaml: &str) -> Self {
        let file: RegistryFile =
            serde_yaml_ng::from_str(yaml).unwrap_or(RegistryFile { models: Vec::new() });
        Registry {
            entries: file.models,
        }
    }

    fn extend_front(&mut self, mut other: Registry) {
        other.entries.append(&mut self.entries);
        self.entries = other.entries;
    }

    /// Best entry for `id`: an exact match beats any prefix; among prefixes the
    /// longest wins; earlier entries (overrides) win ties.
    fn best(&self, id: &str) -> Option<&Entry> {
        let mut best: Option<(&Entry, usize, bool)> = None; // (entry, prefix_len, exact)
        for e in &self.entries {
            let cand = if e.id.as_deref() == Some(id) {
                Some((usize::MAX, true))
            } else if let Some(p) = &e.prefix {
                id.starts_with(p.as_str()).then_some((p.len(), false))
            } else {
                None
            };
            if let Some((len, exact)) = cand {
                let better = match best {
                    None => true,
                    Some((_, blen, _)) => len > blen, // strict: earlier ties keep their slot
                };
                if better {
                    best = Some((e, len, exact));
                }
            }
        }
        best.map(|(e, _, _)| e)
    }

    /// Filled-in default suggestion for `id`.
    pub fn lookup(&self, id: &str) -> ModelDefault {
        let e = self.best(id);
        ModelDefault {
            name: e
                .and_then(|e| e.name.clone())
                .unwrap_or_else(|| derive_name(id)),
            context_window: e.and_then(|e| e.context_window).unwrap_or(FALLBACK_CONTEXT),
            max_tokens: e.and_then(|e| e.max_tokens).unwrap_or(FALLBACK_MAX_TOKENS),
            temperature: e
                .and_then(|e| e.temperature)
                .unwrap_or(FALLBACK_TEMPERATURE),
            reasoning_effort: e.and_then(|e| e.reasoning_effort),
            chat: self.is_chat(id),
        }
    }

    /// Whether `id` is a chat/coding model worth showing in the picker. The
    /// keyword denylist always applies; a matched entry may further exclude
    /// (`chat: false`) but cannot re-include a denylisted id.
    pub fn is_chat(&self, id: &str) -> bool {
        !looks_non_chat(id) && self.best(id).map(|e| e.chat).unwrap_or(true)
    }
}

/// The process-wide merged registry (embedded + local override), loaded once.
fn registry() -> &'static Registry {
    static REG: OnceLock<Registry> = OnceLock::new();
    REG.get_or_init(|| {
        let mut reg = Registry::parse(EMBEDDED);
        if let Some(path) = global_config_dir().map(|d| d.join("model-defaults.yaml")) {
            if let Ok(text) = std::fs::read_to_string(&path) {
                reg.extend_front(Registry::parse(&text));
            }
        }
        reg
    })
}

/// Default suggestion for a model id (embedded table + local override).
pub fn lookup(id: &str) -> ModelDefault {
    registry().lookup(id)
}

/// Whether a model id is a chat/coding model (vs image/tts/audio/embedding/etc).
pub fn is_chat(id: &str) -> bool {
    registry().is_chat(id)
}

/// Keyword denylist for non-chat model families.
fn looks_non_chat(id: &str) -> bool {
    const DENY: &[&str] = &[
        "image",
        "imagen",
        "-tts",
        "tts-",
        "audio",
        "embedding",
        "whisper",
        "moderation",
        "realtime",
        "transcribe",
        "veo",
        "lyria",
        "sora",
        "nano-banana",
        "computer-use",
        "deep-research",
        "robotics",
        "-search",
        "babbage",
        "davinci",
        "-instruct",
        "aqa",
        "flux",
        "live",
    ];
    let lid = id.to_lowercase();
    DENY.iter().any(|k| lid.contains(k))
}

/// Derive a friendly name from a raw id when no entry names it, e.g.
/// `fireworks/accounts/fireworks/models/glm-5p1` -> `Fireworks: glm-5p1`.
pub fn derive_name(id: &str) -> String {
    let provider = id.split('/').next().unwrap_or("model");
    let leaf = id.rsplit('/').next().unwrap_or(id);
    let mut p = provider.chars();
    let provider_title = match p.next() {
        Some(c) => c.to_uppercase().collect::<String>() + p.as_str(),
        None => provider.to_string(),
    };
    format!("{provider_title}: {leaf}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
models:
  - prefix: anthropic/claude-opus
    name: "Claude Opus"
    context_window: 200000
    reasoning_effort: high
  - id: anthropic/claude-opus-4-8
    name: "Claude Opus 4.8"
    context_window: 200000
    temperature: 1.0
    reasoning_effort: high
  - id: cerebras/zai-glm-4.7
    name: "Cerebras: GLM 4.7"
    context_window: 131072
    temperature: 0.6
    reasoning_effort: high
"#;

    #[test]
    fn exact_match_beats_prefix() {
        let r = Registry::parse(SAMPLE);
        let d = r.lookup("anthropic/claude-opus-4-8");
        assert_eq!(d.name, "Claude Opus 4.8");
        assert_eq!(d.temperature, 1.0);
    }

    #[test]
    fn prefix_match_for_unlisted_point_release() {
        let r = Registry::parse(SAMPLE);
        let d = r.lookup("anthropic/claude-opus-4-9");
        assert_eq!(d.name, "Claude Opus"); // from the family prefix
        assert_eq!(d.context_window, 200000);
        assert_eq!(d.reasoning_effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn unknown_id_derives_a_name_and_fallback_settings() {
        let r = Registry::parse(SAMPLE);
        let d = r.lookup("fireworks/accounts/fireworks/models/glm-5p1");
        assert_eq!(d.name, "Fireworks: glm-5p1");
        assert_eq!(d.context_window, FALLBACK_CONTEXT);
        assert_eq!(d.reasoning_effort, None);
    }

    #[test]
    fn chat_filter_excludes_non_chat_families() {
        let r = Registry::parse(SAMPLE);
        assert!(r.is_chat("anthropic/claude-opus-4-8"));
        assert!(r.is_chat("fireworks/.../glm-5p1"));
        assert!(!r.is_chat("openai/gpt-image-1"));
        assert!(!r.is_chat("openai/text-embedding-3-large"));
        assert!(!r.is_chat("gemini/veo-3.0-generate-001"));
        assert!(!r.is_chat("openai/gpt-4o-mini-tts"));
        assert!(!r.is_chat("openai/gpt-5-search-api"));
    }

    #[test]
    fn embedded_table_parses_and_resolves_known_models() {
        let r = Registry::parse(EMBEDDED);
        assert_eq!(r.lookup("cerebras/zai-glm-4.7").name, "Cerebras: GLM 4.7");
        assert_eq!(
            r.lookup("anthropic/claude-opus-4-8").reasoning_effort,
            Some(ReasoningEffort::High)
        );
        // Unlisted point-release inherits the family prefix.
        assert_eq!(r.lookup("openai/gpt-5.2-codex").name, "GPT-5");
        assert!(r.is_chat("fireworks/accounts/fireworks/models/glm-5p1"));
        assert!(!r.is_chat("gemini/imagen-4.0-generate-001"));
    }

    #[test]
    fn override_entries_win_ties() {
        let mut r = Registry::parse(SAMPLE);
        r.extend_front(Registry::parse(
            r#"
models:
  - id: cerebras/zai-glm-4.7
    name: "My GLM"
    temperature: 0.3
"#,
        ));
        let d = r.lookup("cerebras/zai-glm-4.7");
        assert_eq!(d.name, "My GLM");
        assert_eq!(d.temperature, 0.3);
    }
}
