//! Host-managed agent memory.
//!
//! The sandboxed agent can't write the host home directory, but the agent loop
//! runs host-side, so the `memory` tool is handled here directly. Memories are
//! markdown files with YAML frontmatter under `~/.config/cowboy/memory/`,
//! segmented into `global/` (cross-project) and `projects/<key>/` (per worktree,
//! keyed by a hash of the canonical root). The merged index is injected into the
//! agent's context each session; the agent recalls full memories or saves new
//! ones via the tool.
//!
//! Durable *project* conventions belong in the repo's `AGENTS.md`, not here;
//! memory is for the agent's own cross-session notes and user preferences.
//!
//! The `*_in(base, …)` functions take the memory root explicitly so they're
//! testable without touching the real home dir; the public wrappers resolve it
//! from [`crate::config::global_config_dir`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::global_config_dir;
use crate::error::{Error, Result};

/// Which store a memory lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Specific to one worktree.
    Project,
    /// Shared across all projects for this user.
    Global,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::Global => "global",
        }
    }
}

/// Metadata for one stored memory (everything but the body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryMeta {
    pub name: String,
    pub description: String,
    pub scope: Scope,
    pub kind: String,
}

/// The memory root (`~/.config/cowboy/memory`).
fn memory_root() -> Result<PathBuf> {
    global_config_dir()
        .map(|d| d.join("memory"))
        .ok_or_else(|| Error::Invalid("cannot resolve home config directory".into()))
}

fn scope_dir(base: &Path, project_key: &str, scope: Scope) -> PathBuf {
    match scope {
        Scope::Global => base.join("global"),
        Scope::Project => base.join("projects").join(project_key),
    }
}

/// Slugify a title into a filename stem: lowercase, non-alphanumerics collapsed
/// to single dashes, trimmed, capped.
pub fn slugify(title: &str) -> String {
    let mut s: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s: String = s.trim_matches('-').chars().take(60).collect();
    if s.is_empty() {
        "note".into()
    } else {
        s
    }
}

#[derive(Serialize, Deserialize, Default)]
struct Frontmatter {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default, alias = "type")]
    kind: Option<String>,
}

/// Split a memory file into (frontmatter, body).
fn parse(text: &str) -> (Frontmatter, String) {
    let trimmed = text.strip_prefix('\u{feff}').unwrap_or(text);
    if let Some(rest) = trimmed.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") {
            let fm = &rest[..end];
            let after = &rest[end + 4..];
            let body = after.strip_prefix('\n').unwrap_or(after);
            if let Ok(meta) = serde_yaml_ng::from_str::<Frontmatter>(fm) {
                return (meta, body.trim().to_string());
            }
        }
    }
    (Frontmatter::default(), trimmed.trim().to_string())
}

fn read_meta(path: &Path, scope: Scope) -> Option<MemoryMeta> {
    let text = std::fs::read_to_string(path).ok()?;
    let (fm, _) = parse(&text);
    let default_name = path.file_stem()?.to_str()?.to_string();
    Some(MemoryMeta {
        name: fm.name.unwrap_or(default_name),
        description: fm.description.unwrap_or_default(),
        scope,
        kind: fm.kind.unwrap_or_default(),
    })
}

fn list_dir(dir: &Path, scope: Scope) -> Vec<MemoryMeta> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            if let Some(meta) = read_meta(&path, scope) {
                out.push(meta);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

// ---------------------------------------------------------------------------
// Injectable core (base dir passed explicitly)
// ---------------------------------------------------------------------------

/// All memories visible to a project: project entries first, then global ones
/// whose names aren't shadowed by a project entry.
pub fn list_in(base: &Path, project_key: &str) -> Vec<MemoryMeta> {
    let mut out = list_dir(
        &scope_dir(base, project_key, Scope::Project),
        Scope::Project,
    );
    let project_names: std::collections::BTreeSet<_> = out.iter().map(|m| m.name.clone()).collect();
    for g in list_dir(&scope_dir(base, project_key, Scope::Global), Scope::Global) {
        if !project_names.contains(&g.name) {
            out.push(g);
        }
    }
    out
}

/// A one-line-per-memory index for the agent's session context, or empty when
/// there are no memories.
pub fn index_in(base: &Path, project_key: &str) -> String {
    let items = list_in(base, project_key);
    if items.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "Stored memories (recall a full entry with the `memory` tool's `recall` action):\n",
    );
    for m in items {
        let kind = if m.kind.is_empty() {
            String::new()
        } else {
            format!(" ({})", m.kind)
        };
        s.push_str(&format!(
            "- {} — {}  [{}]{kind}\n",
            m.name,
            m.description,
            m.scope.as_str()
        ));
    }
    s
}

/// Create or overwrite a memory; returns its `name` (slug).
pub fn save_in(
    base: &Path,
    project_key: &str,
    title: &str,
    content: &str,
    scope: Scope,
    kind: Option<&str>,
) -> Result<String> {
    let name = slugify(title);
    let dir = scope_dir(base, project_key, scope);
    std::fs::create_dir_all(&dir)?;
    restrict_dir(&dir);
    let kind = kind.unwrap_or("note");
    let doc = format!(
        "---\nname: {name}\ndescription: {}\nscope: {}\ntype: {kind}\n---\n{}\n",
        title.replace('\n', " ").trim(),
        scope.as_str(),
        content.trim_end(),
    );
    let path = dir.join(format!("{name}.md"));
    let tmp = dir.join(format!(".{name}.md.tmp"));
    std::fs::write(&tmp, doc)?;
    std::fs::rename(&tmp, &path)?;
    Ok(name)
}

/// Full body of a memory by name (project store wins over global).
pub fn recall_in(base: &Path, project_key: &str, name: &str) -> Result<Option<String>> {
    for scope in [Scope::Project, Scope::Global] {
        let path = scope_dir(base, project_key, scope).join(format!("{name}.md"));
        if let Ok(text) = std::fs::read_to_string(&path) {
            let (_, body) = parse(&text);
            return Ok(Some(body));
        }
    }
    Ok(None)
}

/// Delete a memory by name from both stores; returns whether anything was
/// removed.
pub fn delete_in(base: &Path, project_key: &str, name: &str) -> Result<bool> {
    let mut removed = false;
    for scope in [Scope::Project, Scope::Global] {
        let path = scope_dir(base, project_key, scope).join(format!("{name}.md"));
        if path.exists() {
            std::fs::remove_file(&path)?;
            removed = true;
        }
    }
    Ok(removed)
}

#[cfg(unix)]
fn restrict_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}
#[cfg(not(unix))]
fn restrict_dir(_dir: &Path) {}

// ---------------------------------------------------------------------------
// Public wrappers (resolve the home memory root)
// ---------------------------------------------------------------------------

pub fn index(project_key: &str) -> String {
    memory_root()
        .map(|base| index_in(&base, project_key))
        .unwrap_or_default()
}

pub fn list(project_key: &str) -> Result<Vec<MemoryMeta>> {
    Ok(list_in(&memory_root()?, project_key))
}

pub fn save(
    project_key: &str,
    title: &str,
    content: &str,
    scope: Scope,
    kind: Option<&str>,
) -> Result<String> {
    save_in(&memory_root()?, project_key, title, content, scope, kind)
}

pub fn recall(project_key: &str, name: &str) -> Result<Option<String>> {
    recall_in(&memory_root()?, project_key, name)
}

pub fn delete(project_key: &str, name: &str) -> Result<bool> {
    delete_in(&memory_root()?, project_key, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "cowboy-mem-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[test]
    fn slugify_is_clean() {
        assert_eq!(slugify("Build uses Just!"), "build-uses-just");
        assert_eq!(slugify("  --weird-- "), "weird");
        assert_eq!(slugify(""), "note");
    }

    #[test]
    fn save_recall_roundtrip_and_overwrite() {
        let base = tmp();
        let name = save_in(
            &base,
            "key1",
            "Build uses just",
            "Run `just test` not cargo.",
            Scope::Project,
            Some("convention"),
        )
        .unwrap();
        assert_eq!(name, "build-uses-just");
        assert_eq!(
            recall_in(&base, "key1", "build-uses-just")
                .unwrap()
                .as_deref(),
            Some("Run `just test` not cargo.")
        );
        // Same title overwrites in place.
        save_in(
            &base,
            "key1",
            "Build uses just",
            "Updated body.",
            Scope::Project,
            None,
        )
        .unwrap();
        assert_eq!(
            recall_in(&base, "key1", "build-uses-just")
                .unwrap()
                .as_deref(),
            Some("Updated body.")
        );
    }

    #[test]
    fn index_merges_project_and_global_and_scopes_are_isolated() {
        let base = tmp();
        save_in(&base, "k1", "Project fact", "p", Scope::Project, None).unwrap();
        save_in(&base, "k1", "Global pref", "g", Scope::Global, None).unwrap();
        // A different project shares global but not the project entry.
        save_in(&base, "k2", "Other proj", "o", Scope::Project, None).unwrap();

        let idx = index_in(&base, "k1");
        assert!(idx.contains("project-fact"));
        assert!(idx.contains("global-pref"));
        assert!(idx.contains("[project]"));
        assert!(idx.contains("[global]"));

        // k2 sees the global one but not k1's project memory.
        let idx2 = index_in(&base, "k2");
        assert!(idx2.contains("global-pref"));
        assert!(!idx2.contains("project-fact"));
        assert!(idx2.contains("other-proj"));
    }

    #[test]
    fn project_overrides_global_by_name() {
        let base = tmp();
        save_in(&base, "k", "Same Name", "global body", Scope::Global, None).unwrap();
        save_in(
            &base,
            "k",
            "Same Name",
            "project body",
            Scope::Project,
            None,
        )
        .unwrap();
        // list_in keeps only the project entry for a shared name.
        let names: Vec<_> = list_in(&base, "k").into_iter().collect();
        let same: Vec<_> = names.iter().filter(|m| m.name == "same-name").collect();
        assert_eq!(same.len(), 1);
        assert_eq!(same[0].scope, Scope::Project);
        // recall prefers the project body.
        assert_eq!(
            recall_in(&base, "k", "same-name").unwrap().as_deref(),
            Some("project body")
        );
    }

    #[test]
    fn delete_removes_from_both_stores() {
        let base = tmp();
        save_in(&base, "k", "Gone", "x", Scope::Project, None).unwrap();
        assert!(delete_in(&base, "k", "gone").unwrap());
        assert!(recall_in(&base, "k", "gone").unwrap().is_none());
        assert!(!delete_in(&base, "k", "gone").unwrap());
    }

    #[test]
    fn empty_index_is_empty_string() {
        assert_eq!(index_in(&tmp(), "k"), "");
    }
}
