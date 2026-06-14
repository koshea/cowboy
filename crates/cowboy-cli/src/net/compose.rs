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

/// Candidate networks the agent could join for a detected project (the implicit
/// default network plus any explicitly declared ones, prefixed by project name).
pub fn candidate_networks(p: &ComposeProject) -> Vec<String> {
    let mut nets = vec![p.default_network.clone()];
    let project = p.default_network.trim_end_matches("_default");
    for n in &p.declared_networks {
        nets.push(format!("{project}_{n}"));
    }
    nets.sort();
    nets.dedup();
    nets
}

/// Detect a Compose project and, on a terminal, prompt the user to approve the
/// agent joining its networks. Approved networks are persisted to
/// `security.yaml` (`networks.compose.approved`). No-op when no Compose file is
/// found or when not attached to a terminal.
pub fn prompt_and_persist(root: &Path) -> anyhow::Result<()> {
    use std::io::{BufRead, IsTerminal, Write};

    let Some(project) = detect(root)? else {
        return Ok(());
    };
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(());
    }

    let paths = cowboy_core::config::ConfigPaths::for_root(root);
    let mut security = match cowboy_core::config::SecurityConfig::load(&paths.security) {
        Ok(s) => s,
        Err(_) => return Ok(()), // no security.yaml yet; nothing to persist into
    };

    println!(
        "\nDetected Docker Compose project ({}): services [{}]",
        project
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?"),
        project.services.join(", ")
    );

    let mut changed = false;
    for net in candidate_networks(&project) {
        if security.networks.compose.approved.contains(&net) {
            continue;
        }
        print!("  Allow the agent container to join network `{net}`? [y/N] ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        if matches!(line.trim(), "y" | "Y" | "yes") {
            security.networks.compose.approved.push(net.clone());
            changed = true;
            println!("    approved {net}");
        }
    }

    if changed {
        security.save(&paths.security)?;
        println!(
            "  updated {} (networks.compose.approved)",
            paths.security.display()
        );
    }
    Ok(())
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

    #[test]
    fn candidate_networks_includes_default_and_declared() {
        let p = ComposeProject {
            path: "compose.yaml".into(),
            services: vec!["web".into()],
            declared_networks: vec!["backend".into()],
            default_network: "myapp_default".into(),
        };
        let nets = candidate_networks(&p);
        assert!(nets.contains(&"myapp_default".to_string()));
        assert!(nets.contains(&"myapp_backend".to_string()));
    }
}
