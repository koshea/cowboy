//! The repo's own agent instructions (`AGENTS.md` / `CLAUDE.md`), as a block that
//! can be pinned into the system prompt.
//!
//! The agent is told these files are authoritative, and before this it had to go and
//! read one to find out what they said. That costs a turn on every session, and the
//! conventions then sit in the middle of the conversation where compaction can fold
//! them away — so a long session drifts back to generic habits precisely when it has
//! accumulated the most code to be consistent with. Pinning the file is the same
//! trade the memory and skill indexes already make.
//!
//! **This is repo content, and therefore untrusted.** That is not a new exposure:
//! it is the same bytes the agent would otherwise `read` itself a turn later, and
//! nothing in cowboy's boundary depends on what the model is told — mounts,
//! network and credentials are enforced host-side against config the agent cannot
//! reach (see `docs/src/security/model.md`). The block is delimited and labelled as
//! project-provided so instructions in it read as the repo's conventions, which is
//! what they are, rather than as the host's rules.

use std::path::Path;

/// Files searched, in precedence order. `AGENTS.md` first because it is the
/// tool-neutral name; `CLAUDE.md` is honoured so a repo already carrying one for
/// Claude Code needs no second file.
const CANDIDATES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// The repo-root instructions block, or the empty string when there is none or
/// `max_bytes` is 0 (the off switch).
///
/// Only the repo root is read. Nested `AGENTS.md` files still belong to the agent to
/// find when it works in a subtree — pinning every one of them would scale with the
/// repo rather than with the task.
pub fn block(root: &Path, max_bytes: usize) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    let Some((name, text)) = read_first(root) else {
        return String::new();
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let (body, cut) = clip(trimmed, max_bytes);
    let mut out = format!(
        "\n\n## {name} (this project's own instructions — authoritative for how work \
         here is done)\n\n{body}\n"
    );
    if cut {
        // Say so explicitly: silently handing over two thirds of a conventions
        // document would leave the agent confident it had read the rules.
        out.push_str(&format!(
            "\n[{name} is longer than this; only the first {} bytes are shown. `read` \
             {name} for the rest before relying on it.]\n",
            body.len()
        ));
    }
    // Mention a second file rather than concatenating it: two conventions documents
    // in one prompt is usually one of them pointing at the other.
    for other in CANDIDATES.iter().filter(|c| **c != name) {
        if root.join(other).is_file() {
            out.push_str(&format!(
                "\n(This repo also has a {other}; `read` it if {name} refers to it.)\n"
            ));
        }
    }
    out
}

fn read_first(root: &Path) -> Option<(&'static str, String)> {
    CANDIDATES.iter().find_map(|name| {
        let p = root.join(name);
        p.is_file()
            .then(|| std::fs::read_to_string(&p).ok())
            .flatten()
            .map(|t| (*name, t))
    })
}

/// Clip to `max_bytes` on a line boundary where possible, so the block never ends
/// mid-sentence or mid-code-fence-line.
fn clip(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let end = match text[..end].rfind('\n') {
        // Only snap back to a line boundary if it does not throw away a lot.
        Some(nl) if nl > end.saturating_sub(2048) => nl,
        _ => end,
    };
    (&text[..end], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        // A counter as well as the time: tests run as threads of one process, and
        // macOS's clock ticks in microseconds, so two could otherwise share a dir.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "cowboy-instr-{}-{n}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn agents_md_is_read_and_labelled_as_the_projects_own() {
        let root = tmp();
        std::fs::write(root.join("AGENTS.md"), "# Rules\n\nRun `just test`.\n").unwrap();
        let out = block(&root, 4096);
        assert!(out.contains("## AGENTS.md"), "{out}");
        assert!(out.contains("authoritative"), "{out}");
        assert!(out.contains("Run `just test`."), "{out}");
    }

    #[test]
    fn claude_md_is_the_fallback_and_agents_md_wins_when_both_exist() {
        let root = tmp();
        std::fs::write(root.join("CLAUDE.md"), "claude rules\n").unwrap();
        assert!(block(&root, 4096).contains("claude rules"));

        std::fs::write(root.join("AGENTS.md"), "agents rules\n").unwrap();
        let out = block(&root, 4096);
        assert!(out.contains("agents rules"), "{out}");
        assert!(!out.contains("claude rules"), "only one is inlined: {out}");
        assert!(
            out.contains("also has a CLAUDE.md"),
            "but it is named: {out}"
        );
    }

    /// A long conventions document is clipped — and says that it was, because an
    /// agent that thinks it has read the rules when it has seen a third of them is
    /// worse off than one that knows to go and look.
    #[test]
    fn an_oversized_file_is_clipped_and_says_so() {
        let root = tmp();
        let body: String = (0..500).map(|i| format!("rule number {i}\n")).collect();
        std::fs::write(root.join("AGENTS.md"), &body).unwrap();
        let out = block(&root, 500);
        assert!(out.contains("rule number 0"), "{out}");
        assert!(!out.contains("rule number 499"), "must be clipped");
        assert!(out.contains("only the first"), "must admit it: {out}");
        assert!(out.contains("`read` AGENTS.md"), "{out}");
    }

    #[test]
    fn no_file_and_a_zero_budget_both_yield_nothing() {
        let root = tmp();
        assert_eq!(block(&root, 4096), "");
        std::fs::write(root.join("AGENTS.md"), "x\n").unwrap();
        assert_eq!(block(&root, 0), "", "0 bytes is the off switch");
    }
}
