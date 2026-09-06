//! Token counting for context-window management.
//!
//! Uses `tiktoken-rs` (cl100k BPE). For non-OpenAI backends this is an
//! approximation, but it is a far better budget signal than `bytes / 4` and is
//! good enough for deciding when to prune conversation history.

use std::sync::OnceLock;

use tiktoken_rs::CoreBPE;

fn bpe() -> &'static CoreBPE {
    static BPE: OnceLock<CoreBPE> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::cl100k_base().expect("cl100k_base BPE loads"))
}

/// Approximate token count of `text`.
///
/// ```
/// assert_eq!(cowboy_core::tokens::count(""), 0);
/// assert!(cowboy_core::tokens::count("hello world") >= 1);
/// ```
pub fn count(text: &str) -> usize {
    bpe().encode_ordinary(text).len()
}

/// Truncate `text` to at most `max_tokens` tokens (decoding the kept prefix).
///
/// `decode` can fail when the cut falls in the middle of a multi-token character
/// (the byte sequence for the kept prefix isn't valid UTF-8 on its own). When it
/// does, we retry with one fewer token until a prefix decodes — every such prefix
/// is ≤ `max_tokens` tokens, so the result never exceeds the budget. The old
/// fallback (`chars().take(max_tokens * 4)`) could return up to ~4× the budget in
/// tokens, defeating the point of a token-exact truncation.
pub fn truncate_to_tokens(text: &str, max_tokens: usize) -> String {
    let toks = bpe().encode_ordinary(text);
    if toks.len() <= max_tokens {
        return text.to_string();
    }
    // Back off from `max_tokens` until a prefix decodes cleanly. Bounded (at most a
    // few iterations in practice — a character spans very few tokens) and always
    // ≤ max_tokens, so it can never overshoot the budget.
    for end in (0..=max_tokens).rev() {
        if let Ok(s) = bpe().decode(&toks[..end]) {
            return s;
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_reasonable() {
        // A handful of words is a handful of tokens (not chars).
        let n = count("the quick brown fox");
        assert!((1..=8).contains(&n), "got {n}");
        assert_eq!(count(""), 0);
    }

    #[test]
    fn truncate_keeps_short_text() {
        assert_eq!(truncate_to_tokens("hello world", 100), "hello world");
    }

    #[test]
    fn truncate_shortens_long_text() {
        let big = "word ".repeat(1000);
        let t = truncate_to_tokens(&big, 10);
        assert!(count(&t) <= 10);
        assert!(t.len() < big.len());
    }

    /// Multibyte text (emoji, CJK) is where a token boundary can split a character
    /// and make `decode` fail. The result must still be within budget — never the
    /// old `chars().take(max*4)` overshoot. (M13)
    #[test]
    fn truncate_never_exceeds_budget_on_multibyte_text() {
        for text in [
            "😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀".to_string(),
            "日本語のテキストをたくさん".repeat(20),
            "café résumé naïve ".repeat(50),
            "🇺🇸🇬🇧🇯🇵".repeat(30), // flag emoji are multi-codepoint
        ] {
            for budget in [0usize, 1, 3, 7, 20] {
                let t = truncate_to_tokens(&text, budget);
                assert!(
                    count(&t) <= budget,
                    "budget {budget}: got {} tokens for {t:?}",
                    count(&t)
                );
            }
        }
    }
}
