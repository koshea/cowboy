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
pub fn truncate_to_tokens(text: &str, max_tokens: usize) -> String {
    let toks = bpe().encode_ordinary(text);
    if toks.len() <= max_tokens {
        return text.to_string();
    }
    bpe()
        .decode(toks[..max_tokens].to_vec())
        .unwrap_or_else(|_| text.chars().take(max_tokens * 4).collect())
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
}
