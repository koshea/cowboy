//! `cowboy models migrate` — rewrite the home and project `models.yaml` and the
//! crew roster from bare model names to `provider/name`.
//!
//! The conversion itself is pure (`cowboy_core::models_migrate`); this side owns
//! the files: it shows a diff, asks, then under the model-config lock re-checks
//! that nothing changed since it read them, writes a `.bak` beside each, and
//! replaces them.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use cowboy_core::config::{ConfigPaths, ModelsConfig, ProvidersConfig};
use cowboy_core::crew;
use cowboy_core::models_migrate::{
    legacy_renames, migrate_crew_file, migrate_models_text, Renames,
};

/// One file the migration would rewrite.
struct Rewrite {
    path: PathBuf,
    before: String,
    after: String,
}

pub fn run(yes: bool) -> Result<()> {
    let user_path = ModelsConfig::user_path().context("cannot resolve home config directory")?;
    let project_path = crate::cmd::project_root()
        .ok()
        .map(|root| ConfigPaths::for_root(root).models);
    let crew_path = crew::path().context("cannot resolve home config directory")?;
    let harnesses =
        cowboy_core::harness::HarnessesConfig::load_user().context("loading harnesses.yaml")?;
    let is_harness = |n: &str| harnesses.get(n).is_some();

    let rewrites = plan(&user_path, project_path.as_deref(), &crew_path, &is_harness)?;
    if rewrites.is_empty() {
        crate::ui::ok("model configuration already uses `provider/name`; nothing to migrate");
        return Ok(());
    }

    for r in &rewrites {
        let diff = similar::TextDiff::from_lines(&r.before, &r.after);
        let name = r.path.display().to_string();
        print!(
            "{}",
            diff.unified_diff()
                .context_radius(2)
                .header(&name, &format!("{name} (migrated)"))
        );
    }
    println!();
    if !yes
        && !crate::prompt::confirm_destructive(&format!(
            "Rewrite {} file(s)? A .bak copy of each is kept",
            rewrites.len()
        ))?
    {
        return Ok(());
    }

    let providers_path =
        ProvidersConfig::global_path().context("cannot resolve home config directory")?;
    cowboy_core::config::with_model_config_lock(&providers_path, &user_path, || {
        apply(&rewrites).map_err(|e| cowboy_core::Error::Invalid(format!("{e:#}")))
    })?;
    for r in &rewrites {
        crate::ui::ok(&format!(
            "migrated {} (backup: {})",
            r.path.display(),
            backup_path(&r.path).display()
        ));
    }
    Ok(())
}

/// Compute every rewrite without touching disk.
fn plan(
    user: &Path,
    project: Option<&Path>,
    crew_path: &Path,
    is_harness: &dyn Fn(&str) -> bool,
) -> Result<Vec<Rewrite>> {
    let read = |p: &Path| -> Result<Option<String>> {
        match std::fs::read_to_string(p) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
        }
    };
    let user_text = read(user)?;
    let project_text = match project {
        Some(p) => read(p)?,
        None => None,
    };

    // One rename table across both files: a project default (or a crew slot) may
    // name a model the user file defines. Project entries win, as at resolution.
    let mut renames = Renames::new();
    for (path, text) in [(Some(user), &user_text), (project, &project_text)] {
        if let (Some(path), Some(text)) = (path, text) {
            renames.extend(
                legacy_renames(text).with_context(|| format!("reading {}", path.display()))?,
            );
        }
    }

    let mut out = Vec::new();
    for (path, text) in [(Some(user), user_text), (project, project_text)] {
        let (Some(path), Some(before)) = (path, text) else {
            continue;
        };
        if let Some(cfg) = migrate_models_text(&before, &renames)
            .with_context(|| format!("migrating {}", path.display()))?
        {
            out.push(Rewrite {
                path: path.to_path_buf(),
                before,
                after: serde_yaml_ng::to_string(&cfg)?,
            });
        }
    }

    if let Some(before) = read(crew_path)? {
        // Qualifies bare model slots and turns a single roster into the crew
        // `default`, editing in place so the roster's comments survive.
        if let Some((after, _)) = migrate_crew_file(&before, &renames, is_harness)
            .with_context(|| format!("migrating {}", crew_path.display()))?
        {
            out.push(Rewrite {
                path: crew_path.to_path_buf(),
                before,
                after,
            });
        }
    }
    Ok(out)
}

fn backup_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

/// Write backups, then the new files. Refuses if any file changed since it was
/// read for the diff the user approved.
fn apply(rewrites: &[Rewrite]) -> Result<()> {
    for r in rewrites {
        let now = std::fs::read_to_string(&r.path)
            .with_context(|| format!("re-reading {}", r.path.display()))?;
        if now != r.before {
            bail!(
                "{} changed while migrating; nothing was written — run the migration again",
                r.path.display()
            );
        }
    }
    for r in rewrites {
        // `copy` keeps the source's permissions, so a backup is never looser.
        std::fs::copy(&r.path, backup_path(&r.path))
            .with_context(|| format!("backing up {}", r.path.display()))?;
    }
    for r in rewrites {
        let tmp = r.path.with_extension("migrate.tmp");
        std::fs::write(&tmp, &r.after).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &r.path)
            .with_context(|| format!("replacing {}", r.path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str =
        "default: glm\nmodels:\n  glm:\n    provider: bifrost\n    model: zai/glm\n  \
                        gpt:\n    provider: chatgpt\n    model: gpt-6\n";
    const CREW: &str = "crew:\n  general: glm\n  deep: gpt\n  review: codex\n";

    #[test]
    fn plans_then_applies_with_backups_and_is_idempotent() {
        let dir = assert_fs::TempDir::new().unwrap();
        let user = dir.path().join("models.yaml");
        let project = dir.path().join("proj-models.yaml");
        let crew_path = dir.path().join("crew.yaml");
        std::fs::write(&user, USER).unwrap();
        std::fs::write(&project, "default: gpt\n").unwrap();
        std::fs::write(&crew_path, CREW).unwrap();
        let harness = |n: &str| n == "codex";

        let rewrites = plan(&user, Some(&project), &crew_path, &harness).unwrap();
        assert_eq!(rewrites.len(), 3);
        apply(&rewrites).unwrap();

        let migrated = ModelsConfig::load(&user).unwrap();
        assert_eq!(migrated.default.unwrap().to_string(), "bifrost/glm");
        let proj = ModelsConfig::load(&project).unwrap();
        assert_eq!(proj.default.unwrap().to_string(), "chatgpt/gpt");
        let crew = std::fs::read_to_string(&crew_path).unwrap();
        assert!(crew.contains("general: bifrost/glm"), "{crew}");
        assert!(crew.contains("deep: chatgpt/gpt"), "{crew}");
        assert!(crew.contains("review: codex"), "{crew}");
        // The single roster became the active crew `default`.
        let f = cowboy_core::crew::parse_file(&crew).unwrap();
        assert_eq!(f.active.as_deref(), Some("default"));
        assert!(f.crews.contains_key("default"));
        assert_eq!(std::fs::read_to_string(backup_path(&user)).unwrap(), USER);
        assert_eq!(
            std::fs::read_to_string(backup_path(&crew_path)).unwrap(),
            CREW
        );

        // A second run finds nothing to do.
        assert!(plan(&user, Some(&project), &crew_path, &harness)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_file_edited_after_the_diff_is_not_overwritten() {
        let dir = assert_fs::TempDir::new().unwrap();
        let user = dir.path().join("models.yaml");
        std::fs::write(&user, USER).unwrap();
        let rewrites = plan(&user, None, &dir.path().join("crew.yaml"), &|_| false).unwrap();
        std::fs::write(&user, format!("{USER}# edited\n")).unwrap();
        assert!(apply(&rewrites).is_err());
        assert!(std::fs::read_to_string(&user)
            .unwrap()
            .ends_with("# edited\n"));
        assert!(!backup_path(&user).exists());
    }
}
