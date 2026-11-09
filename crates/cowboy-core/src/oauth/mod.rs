//! Host-side OAuth credentials for model providers.
//!
//! Everything here lives under `~/.config/cowboy/auth/` — inside the config dir
//! the sandbox denylist already refuses (`DenyReason::ProviderCredentials`), so no
//! grant or mount can expose a token to the agent — and is read only by the
//! host-side worker that makes model requests.

pub mod chatgpt;
pub mod login;

use std::path::PathBuf;

use crate::error::{Error, Result};

/// `~/.config/cowboy/auth`.
pub fn auth_dir() -> Result<PathBuf> {
    crate::config::global_config_dir()
        .map(|d| d.join("auth"))
        .ok_or_else(|| Error::Invalid("cannot resolve the home config directory".into()))
}

/// This machine's stable, opaque agent-host id (`urn:uuid:…`), created on first
/// use and reused for every sign-in after. Not a credential: it only tells the
/// provider which installation a registration's usage comes from.
pub fn host_id(dir: &std::path::Path) -> Result<String> {
    let path = dir.join("host_id");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim();
        if s.starts_with("urn:uuid:") && s.len() > "urn:uuid:".len() {
            return Ok(s.to_string());
        }
    }
    std::fs::create_dir_all(dir)?;
    let id = format!("urn:uuid:{}", uuid::Uuid::new_v4().hyphenated());
    // Exclusive create, so two first sign-ins racing agree on one id.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut f) => {
            use std::io::Write;
            f.write_all(id.as_bytes())?;
            f.sync_all()?;
            Ok(id)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(std::fs::read_to_string(&path)?.trim().to_string())
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_id_is_created_once_and_reused() {
        let dir = std::env::temp_dir().join(format!("cowboy-hostid-{}", uuid::Uuid::new_v4()));
        let a = host_id(&dir).unwrap();
        assert!(a.starts_with("urn:uuid:"), "{a}");
        assert_eq!(host_id(&dir).unwrap(), a);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
