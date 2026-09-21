//! Agent skills: named, reusable capabilities described in `SKILL.md` files.
//!
//! A skill is a directory containing `SKILL.md` (with YAML frontmatter giving a
//! `name` and `description`) plus optional scripts/resources. Skills are
//! discovered from, in precedence order: `.cowboy/skills/` and `.claude/skills/`
//! in the project, then `~/.config/cowboy/skills/` and `~/.claude/skills/`
//! globally. The `.claude/` locations let Cowboy share skills with Claude Code
//! users in the same repo.
//!
//! In keeping with the shell-first design, skills are surfaced via the
//! `cowboy skill` CLI: the agent runs `cowboy skill list` to discover them and
//! `cowboy skill show <name>` to pull a skill's instructions into context, then
//! follows them (running the skill's scripts via the shell).

use std::path::{Path, PathBuf};

/// A discovered skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Usage hint for arguments (Claude Code's `argument-hint` frontmatter), if any.
    pub argument_hint: Option<String>,
    /// The `SKILL.md` body (instructions), with frontmatter stripped.
    pub instructions: String,
    /// The skill's directory (holds scripts/resources).
    pub dir: PathBuf,
    /// True if it came from the global (user) skills directory.
    pub global: bool,
}

/// The project skills directory (`.cowboy/skills/`).
pub fn project_dir(root: &Path) -> PathBuf {
    root.join(".cowboy").join("skills")
}

/// The global (user) skills directory (`~/.config/cowboy/skills/`), if resolvable.
pub fn global_dir() -> Option<PathBuf> {
    crate::config::global_config_dir().map(|d| d.join("skills"))
}

/// The home directory, for resolving `~/.claude/...`.
fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

/// The skill search path, in precedence order. `.claude/` locations are shared
/// with Claude Code; the `global` flag marks user-level (vs project) dirs.
fn search_dirs(root: &Path) -> Vec<(PathBuf, bool)> {
    let mut dirs = vec![
        (project_dir(root), false),
        (root.join(".claude").join("skills"), false),
    ];
    if let Some(g) = global_dir() {
        dirs.push((g, true));
    }
    if let Some(h) = home_dir() {
        dirs.push((h.join(".claude").join("skills"), true));
    }
    dirs
}

/// Discover all skills (project first, then global; `.cowboy` before `.claude`),
/// de-duplicated by name (earlier dirs win).
pub fn discover(root: &Path) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (dir, global) in search_dirs(root) {
        for skill in read_dir(&dir, global) {
            if seen.insert(skill.name.clone()) {
                out.push(skill);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Load a single skill by name.
pub fn load(root: &Path, name: &str) -> Option<Skill> {
    discover(root).into_iter().find(|s| s.name == name)
}

/// Bytes of the skill index worth spending on a pinned prompt block, so a directory
/// of fifty skills cannot crowd out the conversation.
const MAX_INDEX_BYTES: usize = 4096;

/// The one-line-per-skill index handed to the agent at the start of a session, or
/// the empty string when the project has no skills.
///
/// Discovery through `cowboy skill list` alone has a cost that is easy to miss: the
/// agent has to spend a turn to find out whether skills exist, so a prompt that only
/// says they "may be available" gets that turn spent on most sessions and skipped on
/// the rest — which is the worst of both. Names and descriptions are small; the
/// instructions, which are not, still come from `skill show` on demand. This mirrors
/// the memory index, and for the same reason: the cheap half of the lookup belongs in
/// the pinned prompt so it survives compaction.
pub fn index(root: &Path) -> String {
    index_of(&discover(root))
}

/// The index for an explicit skill set. Split from [`index`] so it can be tested
/// without the developer's own global skills (`~/.config/cowboy/skills`,
/// `~/.claude/skills`) leaking into the assertion — they are legitimately part of
/// `discover`, and they made the first version of the empty-case test fail on any
/// machine that had one.
fn index_of(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "Skills available for this project (read one with `cowboy skill show <name>` \
         before doing that kind of work, then follow it):\n",
    );
    for skill in skills {
        let scope = if skill.global { " (global)" } else { "" };
        let line = format!("- {} — {}{scope}\n", skill.name, skill.description.trim());
        if s.len() + line.len() > MAX_INDEX_BYTES {
            s.push_str("- … more; run `cowboy skill list` for the full list\n");
            break;
        }
        s.push_str(&line);
    }
    s
}

fn read_dir(dir: &Path, global: bool) -> Vec<Skill> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // `<dir>/<name>/SKILL.md`
        let md = if path.is_dir() {
            path.join("SKILL.md")
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            // Also accept a flat `<name>.md`.
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
            .unwrap_or("skill")
            .to_string();
        out.push(Skill {
            name: parsed.name.unwrap_or(default_name),
            description: parsed.description.unwrap_or_default(),
            argument_hint: parsed.argument_hint,
            instructions: parsed.body,
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
    argument_hint: Option<String>,
    body: String,
}

#[derive(serde::Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(rename = "argument-hint")]
    argument_hint: Option<String>,
}

/// Parse `SKILL.md`: optional `---`-delimited YAML frontmatter, then the body.
fn parse(text: &str) -> Parsed {
    let trimmed = text.strip_prefix('\u{feff}').unwrap_or(text);
    if let Some(rest) = trimmed.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") {
            let fm = &rest[..end];
            // Body starts after the closing `---` line.
            let after = &rest[end + 4..];
            let body = after.strip_prefix('\n').unwrap_or(after).to_string();
            if let Ok(meta) = serde_yaml_ng::from_str::<Frontmatter>(fm) {
                return Parsed {
                    name: meta.name,
                    description: meta.description,
                    argument_hint: meta.argument_hint,
                    body: body.trim_start().to_string(),
                };
            }
        }
    }
    Parsed {
        name: None,
        description: None,
        argument_hint: None,
        body: trimmed.trim_start().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        let sd = dir.join(".cowboy").join("skills").join(name);
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::write(sd.join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn parses_frontmatter_and_body() {
        let p = parse("---\nname: deploy\ndescription: ship it\n---\nStep 1. do x\n");
        assert_eq!(p.name.as_deref(), Some("deploy"));
        assert_eq!(p.description.as_deref(), Some("ship it"));
        assert_eq!(p.body, "Step 1. do x\n");
    }

    #[test]
    fn body_without_frontmatter() {
        let p = parse("just instructions\n");
        assert!(p.name.is_none());
        assert_eq!(p.body, "just instructions\n");
    }

    #[test]
    fn discovers_project_skills() {
        let tmp = tempdir();
        write(
            tmp.path(),
            "review",
            "---\nname: review\ndescription: review code\n---\nRun the linter.\n",
        );
        write(tmp.path(), "noname", "no frontmatter here\n");
        let skills = discover(tmp.path());
        let review = skills.iter().find(|s| s.name == "review").unwrap();
        assert_eq!(review.description, "review code");
        assert!(review.instructions.contains("Run the linter"));
        // A skill dir without frontmatter falls back to its directory name.
        assert!(skills.iter().any(|s| s.name == "noname"));
    }

    /// The index is what makes skills discoverable without spending a turn on
    /// `cowboy skill list`: names and descriptions only, and nothing at all when there
    /// are no skills (an empty header would be pure overhead on every request).
    #[test]
    fn the_index_lists_names_and_descriptions_and_is_empty_without_skills() {
        assert_eq!(index_of(&[]), "");

        let tmp = tempdir();
        write(
            tmp.path(),
            "review",
            "---\nname: review\ndescription: review code\n---\nRun the linter.\n",
        );
        let idx = index(tmp.path());
        assert!(idx.contains("- review — review code"), "{idx}");
        assert!(
            idx.contains("cowboy skill show"),
            "says how to read one: {idx}"
        );
        // The instructions themselves are *not* pinned — that is the expensive half.
        assert!(!idx.contains("Run the linter"), "{idx}");
    }

    /// Many skills must not crowd out the conversation the index is meant to help;
    /// past the budget it says so and points at the CLI.
    #[test]
    fn a_huge_skill_directory_is_bounded() {
        let many: Vec<Skill> = (0..200)
            .map(|i| Skill {
                name: format!("skill{i:03}"),
                description: "a rather wordy description of what this one does ".repeat(3),
                argument_hint: None,
                instructions: String::new(),
                dir: std::path::PathBuf::new(),
                global: false,
            })
            .collect();
        let idx = index_of(&many);
        assert!(
            idx.len() <= MAX_INDEX_BYTES + 128,
            "got {} bytes",
            idx.len()
        );
        assert!(idx.contains("cowboy skill list"), "{idx}");
    }

    #[test]
    fn discovers_claude_skills_with_namespaced_name() {
        // A Claude Code skill in `.claude/skills/` with a namespaced (colon) name.
        let tmp = tempdir();
        let sd = tmp
            .path()
            .join(".claude")
            .join("skills")
            .join("github:review-pr");
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::write(
            sd.join("SKILL.md"),
            "---\nname: \"github:review-pr\"\ndescription: review a PR\nuser-invocable: true\n---\nDo the review.\n",
        )
        .unwrap();
        let s = load(tmp.path(), "github:review-pr").expect("claude skill discovered");
        assert_eq!(s.description, "review a PR");
        assert!(s.instructions.contains("Do the review"));
    }

    #[test]
    fn cowboy_skills_take_precedence_over_claude() {
        let tmp = tempdir();
        write(
            tmp.path(),
            "dup",
            "---\nname: dup\ndescription: cowboy one\n---\nx\n",
        );
        let cd = tmp.path().join(".claude").join("skills").join("dup");
        std::fs::create_dir_all(&cd).unwrap();
        std::fs::write(
            cd.join("SKILL.md"),
            "---\nname: dup\ndescription: claude one\n---\ny\n",
        )
        .unwrap();
        let s = load(tmp.path(), "dup").unwrap();
        assert_eq!(s.description, "cowboy one"); // .cowboy/skills wins
    }

    #[test]
    fn load_by_name() {
        let tmp = tempdir();
        write(
            tmp.path(),
            "fix",
            "---\nname: fix\ndescription: d\n---\nbody\n",
        );
        assert!(load(tmp.path(), "fix").is_some());
        assert!(load(tmp.path(), "missing").is_none());
    }

    // tiny tempdir without an extra dependency
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
            "cowboy-skills-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
}
