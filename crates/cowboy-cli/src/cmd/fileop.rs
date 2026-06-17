//! `cowboy x-fileop` — the in-container worker behind the structured file tools
//! (`read`/`edit`/`write`). It reads a JSON request on stdin and performs the
//! operation on the workspace, printing the result to stdout. Running inside the
//! container keeps file edits within the Docker boundary (the host never writes
//! the agent's files directly), consistent with cowboy's security model.
//!
//! This command is hidden from `--help`; the host invokes it via
//! `AgentRuntime::fileop`.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// Cap on lines returned by a `read` with no explicit limit.
const DEFAULT_READ_LINES: usize = 2000;

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum FileOp {
    Read {
        path: String,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },
    Edit {
        path: String,
        old: String,
        new: String,
        #[serde(default)]
        replace_all: bool,
    },
    Write {
        path: String,
        content: String,
    },
}

pub fn run() -> Result<()> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("reading fileop request from stdin")?;
    let op: FileOp = serde_json::from_str(&input).context("parsing fileop request")?;

    // Resolve paths against the workspace (the container workdir / cwd).
    let root = std::env::current_dir().context("resolving workspace dir")?;

    let out = match op {
        FileOp::Read {
            path,
            offset,
            limit,
        } => read(&root, &path, offset, limit)?,
        FileOp::Edit {
            path,
            old,
            new,
            replace_all,
        } => edit(&root, &path, &old, &new, replace_all)?,
        FileOp::Write { path, content } => write(&root, &path, &content)?,
    };
    print!("{out}");
    Ok(())
}

fn read(root: &Path, path: &str, offset: Option<usize>, limit: Option<usize>) -> Result<String> {
    let p = resolve(root, path)?;
    let text = std::fs::read_to_string(&p).with_context(|| format!("reading {path}"))?;
    let start = offset.unwrap_or(1).max(1); // 1-based
    let limit = limit.unwrap_or(DEFAULT_READ_LINES);
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate().skip(start - 1).take(limit) {
        out.push_str(&format!("{:>6}\t{}\n", i + 1, line));
    }
    let shown_end = (start - 1 + limit).min(total);
    if total == 0 {
        out.push_str("(empty file)\n");
    } else if shown_end < total {
        out.push_str(&format!(
            "… {} more line(s); read with offset={} to continue\n",
            total - shown_end,
            shown_end + 1
        ));
    }
    Ok(out)
}

fn edit(root: &Path, path: &str, old: &str, new: &str, replace_all: bool) -> Result<String> {
    let p = resolve(root, path)?;
    let text = std::fs::read_to_string(&p).with_context(|| format!("reading {path}"))?;
    if old.is_empty() {
        bail!("`old` must not be empty; use the write tool to create or overwrite a file");
    }
    let count = text.matches(old).count();
    if count == 0 {
        bail!("`old` string not found in {path}; read the file and copy the exact text");
    }
    if count > 1 && !replace_all {
        bail!(
            "`old` string is not unique in {path} ({count} matches); \
             include more surrounding context, or set replace_all=true"
        );
    }
    let updated = if replace_all {
        text.replace(old, new)
    } else {
        text.replacen(old, new, 1)
    };
    std::fs::write(&p, updated).with_context(|| format!("writing {path}"))?;
    Ok(format!(
        "edited {path}: {count} replacement{}\n",
        if count == 1 { "" } else { "s" }
    ))
}

fn write(root: &Path, path: &str, content: &str) -> Result<String> {
    let p = resolve(root, path)?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dirs for {path}"))?;
    }
    let existed = p.exists();
    std::fs::write(&p, content).with_context(|| format!("writing {path}"))?;
    Ok(format!(
        "{} {path} ({} bytes)\n",
        if existed { "overwrote" } else { "created" },
        content.len()
    ))
}

/// Resolve `path` against the workspace `root`, confining it to the workspace
/// (lexically — Docker already isolates the container filesystem). Accepts
/// workspace-relative paths and `/workspace`-prefixed absolute paths.
/// Resolve a workspace-relative (or `/workspace`-prefixed) path to a host path,
/// rejecting absolute paths and any `..` that escapes `root`. Shared with the
/// host-side diff reader so both confine identically.
pub(crate) fn resolve(root: &Path, path: &str) -> Result<PathBuf> {
    let workspace = root.to_string_lossy();
    let rel = if let Some(r) = path.strip_prefix(&format!("{workspace}/")) {
        r
    } else if path == workspace.as_ref() {
        bail!("path is required");
    } else if path.starts_with('/') {
        bail!("absolute paths outside the workspace are not allowed: {path:?}");
    } else {
        path
    };
    if rel.is_empty() {
        bail!("path is required");
    }
    let mut out = root.to_path_buf();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() || !out.starts_with(root) {
                    bail!("path {path:?} escapes the workspace");
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("absolute paths outside the workspace are not allowed: {path:?}")
            }
        }
    }
    if !out.starts_with(root) {
        bail!("path {path:?} escapes the workspace");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "cowboy-fileop-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[test]
    fn resolve_accepts_relative_and_workspace_prefixed() {
        let root = tmp();
        assert_eq!(resolve(&root, "src/a.rs").unwrap(), root.join("src/a.rs"));
        // The container path prefix is stripped.
        let root_str = root.to_string_lossy();
        assert_eq!(
            resolve(&root, &format!("{root_str}/src/a.rs")).unwrap(),
            root.join("src/a.rs")
        );
        assert_eq!(
            resolve(&root, "a/./b/../c.rs").unwrap(),
            root.join("a/c.rs")
        );
    }

    #[test]
    fn resolve_rejects_escapes() {
        let root = tmp();
        assert!(resolve(&root, "../escape").is_err());
        assert!(resolve(&root, "a/../../escape").is_err());
        assert!(resolve(&root, "/etc/passwd").is_err());
        assert!(resolve(&root, "").is_err());
    }

    #[test]
    fn write_then_read_roundtrips_with_line_numbers() {
        let root = tmp();
        let msg = write(&root, "dir/hello.txt", "one\ntwo\nthree\n").unwrap();
        assert!(msg.contains("created"));
        let out = read(&root, "dir/hello.txt", None, None).unwrap();
        assert!(out.contains("     1\tone"));
        assert!(out.contains("     3\tthree"));
        // Overwrite reports differently.
        let msg = write(&root, "dir/hello.txt", "x\n").unwrap();
        assert!(msg.contains("overwrote"));
    }

    #[test]
    fn read_offset_and_limit_window() {
        let root = tmp();
        let body: String = (1..=10).map(|i| format!("L{i}\n")).collect();
        write(&root, "f.txt", &body).unwrap();
        let out = read(&root, "f.txt", Some(3), Some(2)).unwrap();
        assert!(out.contains("     3\tL3"));
        assert!(out.contains("     4\tL4"));
        assert!(!out.contains("\tL5"));
        assert!(out.contains("more line(s)"));
    }

    #[test]
    fn edit_requires_unique_match_unless_replace_all() {
        let root = tmp();
        write(&root, "f.txt", "a\nDUP\nb\nDUP\n").unwrap();
        // Not unique -> error.
        assert!(edit(&root, "f.txt", "DUP", "X", false).is_err());
        // Missing -> error.
        assert!(edit(&root, "f.txt", "NOPE", "X", false).is_err());
        // replace_all -> both replaced.
        let msg = edit(&root, "f.txt", "DUP", "X", true).unwrap();
        assert!(msg.contains("2 replacements"));
        let body = std::fs::read_to_string(root.join("f.txt")).unwrap();
        assert_eq!(body, "a\nX\nb\nX\n");
    }

    #[test]
    fn edit_unique_match_replaces_once() {
        let root = tmp();
        write(&root, "f.txt", "hello world\n").unwrap();
        let msg = edit(&root, "f.txt", "world", "cowboy", false).unwrap();
        assert!(msg.contains("1 replacement"));
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "hello cowboy\n"
        );
    }
}
