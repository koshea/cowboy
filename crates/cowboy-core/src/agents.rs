//! Agent definitions: named specialist personas in Markdown with YAML
//! frontmatter (`name`, `description`, optional `model`) and a body that is the
//! agent's system prompt / review approach.
//!
//! Discovered from, in precedence order: `.cowboy/agents/` and `.claude/agents/`
//! in the project, then `~/.config/cowboy/agents/` and `~/.claude/agents/`
//! globally. The `.claude/` locations let Cowboy reuse the same agent
//! definitions as Claude Code users in the same repo (a flat `<name>.md` per
//! agent). A subagent can adopt one by passing `agent: <name>`; the crew still
//! owns model routing.

use std::path::{Path, PathBuf};

/// A discovered agent definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub name: String,
    pub description: String,
    /// The body (system prompt / instructions), with frontmatter stripped.
    pub instructions: String,
    /// The model named in the agent's frontmatter, if any (advisory — Cowboy
    /// routes via the crew roster, so this is only used if it resolves).
    pub model: Option<String>,
    /// Where it came from (the containing dir).
    pub dir: PathBuf,
    pub global: bool,
}

fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

/// The agent search path, in precedence order.
fn search_dirs(root: &Path) -> Vec<(PathBuf, bool)> {
    let mut dirs = vec![
        (root.join(".cowboy").join("agents"), false),
        (root.join(".claude").join("agents"), false),
    ];
    if let Some(b) = directories::BaseDirs::new() {
        dirs.push((b.config_dir().join("cowboy").join("agents"), true));
    }
    if let Some(h) = home_dir() {
        dirs.push((h.join(".claude").join("agents"), true));
    }
    dirs
}

/// Discover all agent definitions (earlier dirs win on name collision).
pub fn discover(root: &Path) -> Vec<Agent> {
    let mut out: Vec<Agent> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (dir, global) in search_dirs(root) {
        for agent in read_dir(&dir, global) {
            if seen.insert(agent.name.clone()) {
                out.push(agent);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Load a single agent by name.
pub fn load(root: &Path, name: &str) -> Option<Agent> {
    discover(root).into_iter().find(|a| a.name == name)
}

fn read_dir(dir: &Path, global: bool) -> Vec<Agent> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Claude-style flat `<name>.md`, or a `<name>/AGENT.md` directory.
        let md = if path.is_dir() {
            path.join("AGENT.md")
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            path.clone()
        } else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&md) else {
            continue;
        };
        let parsed = parse(&text);
        let default_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("agent")
            .to_string();
        out.push(Agent {
            name: parsed.name.unwrap_or(default_name),
            description: parsed.description.unwrap_or_default(),
            instructions: parsed.body,
            model: parsed.model,
            dir: if path.is_dir() {
                path
            } else {
                dir.to_path_buf()
            },
            global,
        });
    }
    out
}

struct Parsed {
    name: Option<String>,
    description: Option<String>,
    model: Option<String>,
    body: String,
}

#[derive(serde::Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    model: Option<String>,
}

/// Parse optional `---`-delimited YAML frontmatter, then the body.
fn parse(text: &str) -> Parsed {
    let trimmed = text.trim_start_matches('\u{feff}');
    if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let fm = &rest[..end];
            let body_start = end + "\n---".len();
            let body = rest[body_start..]
                .trim_start_matches(['\r', '\n'])
                .to_string();
            if let Ok(meta) = serde_yaml_ng::from_str::<Frontmatter>(fm) {
                return Parsed {
                    name: meta.name,
                    description: meta.description,
                    model: meta.model,
                    body,
                };
            }
        }
    }
    Parsed {
        name: None,
        description: None,
        model: None,
        body: trimmed.trim_start().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct TempDir(PathBuf);
    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tempdir() -> TempDir {
        let p = std::env::temp_dir().join(format!(
            "cowboy-agents-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    #[test]
    fn discovers_claude_flat_agents() {
        let tmp = tempdir();
        let ad = tmp.path().join(".claude").join("agents");
        std::fs::create_dir_all(&ad).unwrap();
        std::fs::write(
            ad.join("security-reviewer.md"),
            "---\nname: security-reviewer\ndescription: finds auth bugs\nmodel: sonnet\n---\nReview for security issues.\n",
        )
        .unwrap();
        let a = load(tmp.path(), "security-reviewer").expect("agent discovered");
        assert_eq!(a.description, "finds auth bugs");
        assert_eq!(a.model.as_deref(), Some("sonnet"));
        assert!(a.instructions.contains("Review for security"));
        // Falls back to the file stem when frontmatter omits a name.
        std::fs::write(ad.join("doc-reviewer.md"), "Review the docs.\n").unwrap();
        assert!(discover(tmp.path())
            .iter()
            .any(|a| a.name == "doc-reviewer"));
    }
}
