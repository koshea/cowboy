//! Agent skills: named, reusable capabilities described in `SKILL.md` files.
//!
//! A skill is a directory containing `SKILL.md` (with YAML frontmatter giving a
//! `name` and `description`) plus optional scripts/resources. Skills live under
//! `.cowboy/skills/` in the project and `~/.config/cowboy/skills/` globally.
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
    /// The `SKILL.md` body (instructions), with frontmatter stripped.
    pub instructions: String,
    /// The skill's directory (holds scripts/resources).
    pub dir: PathBuf,
    /// True if it came from the global (user) skills directory.
    pub global: bool,
}

/// The project skills directory.
pub fn project_dir(root: &Path) -> PathBuf {
    root.join(".cowboy").join("skills")
}

/// The global (user) skills directory, if a config dir is resolvable.
pub fn global_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.config_dir().join("cowboy").join("skills"))
}

/// Discover all skills (project first, then global), de-duplicated by name
/// (project overrides global).
pub fn discover(root: &Path) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (dir, global) in [(Some(project_dir(root)), false), (global_dir(), true)] {
        let Some(dir) = dir else { continue };
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
    body: String,
}

#[derive(serde::Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
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
                    body: body.trim_start().to_string(),
                };
            }
        }
    }
    Parsed {
        name: None,
        description: None,
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
