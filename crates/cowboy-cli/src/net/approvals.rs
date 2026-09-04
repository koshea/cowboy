//! Persisted network approvals.
//!
//! `project`/`global`-scoped approvals from the TUI are saved here and merged
//! into the network policy at session start, so the gateway allows them in
//! future sessions. We keep this separate from `security.yaml` so we never
//! rewrite the user's commented host config.
//!
//! SECURITY: these are stored **host-side**, under `~/.config/cowboy/approvals/`
//! (keyed by project), NOT inside the project workspace. The workspace is
//! bind-mounted read-write into the agent container, so an approvals file there
//! would let a malicious model/repo widen its own network allow-list by writing
//! the file — the agent must never be able to grant itself egress.

use std::path::{Path, PathBuf};

use cowboy_core::config::NetworkPolicy;
use cowboy_core::netproto::NetworkAttempt;

/// One persisted approval: a host or CIDR, and the port it was approved for.
///
/// The same type the policy evaluates ([`cowboy_core::config::ApprovedEndpoint`]), so
/// there is one definition of "an approved destination" and the stored JSON and the
/// in-memory policy cannot drift apart. The file format is unchanged.
pub type Approval = cowboy_core::config::ApprovedEndpoint;

/// The host-only directory holding per-project approvals
/// (`~/.config/cowboy/approvals/`); falls back to the host temp dir if there is
/// no home config dir. Never inside the (agent-writable) workspace.
fn approvals_dir() -> PathBuf {
    cowboy_core::config::global_config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("approvals")
}

/// Approvals file for a project root within `dir` (keyed by the root's hash).
fn file_in(dir: &Path, root: &Path) -> PathBuf {
    dir.join(format!(
        "{:08x}.json",
        super::super::project::project_hash(root)
    ))
}

/// Load persisted approvals (empty if none).
pub fn load(root: &Path) -> Vec<Approval> {
    load_in(&approvals_dir(), root)
}

/// Append an approval derived from an attempt, de-duplicating.
pub fn append(root: &Path, attempt: &NetworkAttempt) -> std::io::Result<()> {
    append_in(&approvals_dir(), root, attempt)
}

fn load_in(dir: &Path, root: &Path) -> Vec<Approval> {
    std::fs::read_to_string(file_in(dir, root))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn append_in(dir: &Path, root: &Path, attempt: &NetworkAttempt) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    // Serialise the whole read-modify-write. Two approvals racing — two parallel
    // commands in one session, or two sessions on one project — each loaded the same
    // baseline, appended their own entry, and the second write clobbered the first.
    // The `contains` check below is per-call, so the lost approval was silent: the user
    // approved a destination and it simply was not there next session. The lock is a
    // sibling file rather than the approvals file itself, so the exclusive lock is not
    // dropped by the rewrite.
    let _lock = lock(&dir.join(".lock"))?;

    let mut all = load_in(dir, root);
    let entry = Approval {
        host: attempt.host.clone(),
        cidr: attempt
            .host
            .is_none()
            .then(|| attempt.ip.map(|ip| format!("{ip}/32")))
            .flatten(),
        port: attempt.port,
    };
    if !all.contains(&entry) {
        all.push(entry);
        write_private(&file_in(dir, root), &serde_json::to_string_pretty(&all)?)?;
    }
    Ok(())
}

/// Write owner-only.
///
/// The approvals file is the list of destinations this project may reach. Created at
/// the process umask it is world-readable on a typical host, which hands any local
/// user a map of the project's egress — and it sits next to `providers.yaml` in the
/// host config dir, so it should be no looser than its neighbours.
///
/// Created via `OpenOptions` with the mode set, not chmod'ed afterwards, so it is
/// never briefly readable. An existing file keeps its inode, so the mode is also
/// re-applied to tighten one written by an older version.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// An exclusive advisory lock held until the returned file is dropped.
fn lock(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    // SAFETY: flock on a descriptor we own; blocking, and these writes are tiny.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

/// Merge persisted approvals into a policy's allow-list (used when generating
/// the gateway policy file).
/// Merge persisted approvals into a policy for this session.
///
/// Each approval becomes an [`ApprovedEndpoint`] carrying **its own port**, rather than
/// being decomposed into the policy-wide `allow` rule set. That decomposition was wrong
/// in both directions, because `allow` is a cross product of its domains, CIDRs and
/// ports:
///
/// - **It widened.** Approving `evil.example:22` pushed `22` into `allow.ports`, and
///   since [`cowboy_core::policy`] checks the port against that shared list, port 22
///   then opened for *every* allowed domain — `github.com:22` included.
/// - **It silently did nothing.** With the default empty `allow.ports`, the push was
///   skipped entirely and the port fell back to the built-in web ports (80/443). So
///   approving `host:8443` had no effect at all, and the next attempt asked again.
///
/// An approval is the answer to one question about one destination, and is now stored
/// as exactly that.
pub fn merge_into(policy: &mut NetworkPolicy, approvals: &[Approval]) {
    for a in approvals {
        if !policy.approved.contains(a) {
            policy.approved.push(a.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::netproto::{Protocol, Verdict};

    #[test]
    fn append_and_load_roundtrip() {
        // Use an explicit (temp) approvals dir so the test never touches the real
        // host config dir — mirrors the host-only storage location.
        let cfg = assert_fs::TempDir::new().unwrap();
        let proj = assert_fs::TempDir::new().unwrap();
        let attempt = NetworkAttempt {
            protocol: Protocol::Tls,
            host: Some("example.com".into()),
            ip: None,
            port: 443,
            command_pid: None,
        };
        append_in(cfg.path(), proj.path(), &attempt).unwrap();
        append_in(cfg.path(), proj.path(), &attempt).unwrap(); // dedup
        let all = load_in(cfg.path(), proj.path());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].host.as_deref(), Some("example.com"));
        // The approvals file is under the config dir, not the project workspace.
        assert!(!proj.path().join(".cowboy/approvals.json").exists());
    }

    /// The list of destinations a project may reach is not other users' business, and
    /// it lives beside `providers.yaml` — so it must be no looser than its neighbours.
    /// `fs::write` created it at the process umask, i.e. world-readable on a typical
    /// host.
    #[test]
    fn the_approvals_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let cfg = assert_fs::TempDir::new().unwrap();
        let proj = assert_fs::TempDir::new().unwrap();
        append_in(cfg.path(), proj.path(), &attempt("example.com")).unwrap();
        let path = file_in(cfg.path(), proj.path());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    /// Concurrent approvals must all survive.
    ///
    /// Each one used to load the same baseline, append its own entry and write the
    /// whole list back, so the last writer won and the others vanished — silently, and
    /// exactly when the user was busy approving several destinations at once (two
    /// parallel commands, or two sessions on one project).
    #[test]
    fn concurrent_approvals_do_not_clobber_each_other() {
        let cfg = assert_fs::TempDir::new().unwrap();
        let proj = assert_fs::TempDir::new().unwrap();
        let hosts: Vec<String> = (0..12).map(|i| format!("host{i}.test")).collect();
        std::thread::scope(|s| {
            for h in &hosts {
                s.spawn(|| append_in(cfg.path(), proj.path(), &attempt(h)).unwrap());
            }
        });
        let all = load_in(cfg.path(), proj.path());
        let saved: std::collections::BTreeSet<_> =
            all.iter().filter_map(|a| a.host.clone()).collect();
        for h in &hosts {
            assert!(saved.contains(h), "approval for {h} was lost: {saved:?}");
        }
    }

    fn attempt(host: &str) -> NetworkAttempt {
        NetworkAttempt {
            protocol: Protocol::Tls,
            host: Some(host.to_string()),
            ip: None,
            port: 443,
            command_pid: None,
        }
    }

    /// Approvals reach the policy as scoped endpoints, and leave the hand-written
    /// `allow` rule set alone.
    ///
    /// This test used to assert the opposite — that `a.test` landed in `allow.domains`
    /// and `53` in `allow.ports`. That last assertion was the bug written down as
    /// intent: `allow.ports` is shared by every domain and CIDR in the rule set, so
    /// approving one host on port 53 opened port 53 for all of them.
    #[test]
    fn approvals_merge_as_scoped_endpoints_not_into_the_allow_rules() {
        let mut policy = NetworkPolicy::default();
        policy.allow.domains = vec!["github.com".into()];
        policy.allow.ports = vec![443];
        let before_allow = policy.allow.clone();

        let approvals = [
            Approval {
                host: Some("a.test".into()),
                cidr: None,
                port: 443,
            },
            Approval {
                host: None,
                cidr: Some("9.9.9.9/32".into()),
                port: 53,
            },
        ];
        merge_into(&mut policy, &approvals);

        assert_eq!(
            policy.allow, before_allow,
            "merging approvals must not rewrite the policy's own allow rules"
        );
        assert_eq!(policy.approved, approvals);

        // Each is honoured on its own port…
        let (v, _) = cowboy_core::policy::evaluate(&policy, &attempt_on("a.test", None, 443));
        assert_eq!(v, Verdict::Allow);
        let (v, _) = cowboy_core::policy::evaluate(&policy, &attempt_on("x", Some("9.9.9.9"), 53));
        assert_eq!(v, Verdict::Allow);
        // …and port 53 did not leak to the allow-listed domain.
        let (v, _) = cowboy_core::policy::evaluate(&policy, &attempt_on("github.com", None, 53));
        assert_eq!(
            v,
            Verdict::Ask,
            "approving one host on port 53 must not open port 53 for github.com"
        );
    }

    /// Merging twice must not accumulate duplicates — `merge_into` runs at every
    /// session start against the same stored file.
    #[test]
    fn merging_the_same_approvals_twice_is_idempotent() {
        let mut policy = NetworkPolicy::default();
        let approvals = [Approval {
            host: Some("a.test".into()),
            cidr: None,
            port: 443,
        }];
        merge_into(&mut policy, &approvals);
        merge_into(&mut policy, &approvals);
        assert_eq!(policy.approved.len(), 1);
    }

    fn attempt_on(host: &str, ip: Option<&str>, port: u16) -> NetworkAttempt {
        NetworkAttempt {
            protocol: Protocol::Tls,
            host: Some(host.to_string()),
            ip: ip.map(|s| s.parse().unwrap()),
            port,
            command_pid: None,
        }
    }
}
