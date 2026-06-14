//! Docker Compose file detection and lightweight parsing.
//!
//! We only need enough to discover service and network names so the user can
//! approve the agent joining a Compose network. We deserialize a small subset
//! of the schema ourselves rather than pulling a heavy typed-compose crate
//! (those still depend on the archived `serde_yaml`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Candidate Compose filenames, in precedence order.
pub const CANDIDATES: &[&str] = &[
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

/// A detected Compose project: its file plus discovered services and networks.
#[derive(Debug, Clone, PartialEq)]
pub struct ComposeProject {
    pub path: PathBuf,
    pub services: Vec<String>,
    /// Explicitly declared network names from the top-level `networks:` key.
    pub declared_networks: Vec<String>,
    /// The implicit default network Docker creates: `<project>_default`.
    pub default_network: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawCompose {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    services: BTreeMap<String, serde_yaml_ng::Value>,
    #[serde(default)]
    networks: BTreeMap<String, serde_yaml_ng::Value>,
}

/// Find the first Compose file in `root`, if any.
pub fn find(root: &Path) -> Option<PathBuf> {
    CANDIDATES
        .iter()
        .map(|name| root.join(name))
        .find(|p| p.exists())
}

/// Detect and parse the Compose project rooted at `root`.
pub fn detect(root: &Path) -> anyhow::Result<Option<ComposeProject>> {
    let Some(path) = find(root) else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(&path)?;
    let raw: RawCompose = serde_yaml_ng::from_str(&text)?;

    let project = raw
        .name
        .clone()
        .or_else(|| project_name_from_dir(root))
        .unwrap_or_else(|| "compose".to_string());
    let project = sanitize_project_name(&project);

    Ok(Some(ComposeProject {
        path,
        services: raw.services.keys().cloned().collect(),
        declared_networks: raw.networks.keys().cloned().collect(),
        default_network: format!("{project}_default"),
    }))
}

fn project_name_from_dir(root: &Path) -> Option<String> {
    root.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
}

/// Docker derives the project name by lowercasing and stripping characters
/// outside `[a-z0-9_-]`.
fn sanitize_project_name(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_project_names() {
        assert_eq!(sanitize_project_name("My App!"), "myapp");
        assert_eq!(sanitize_project_name("my_proj-1"), "my_proj-1");
    }
}
