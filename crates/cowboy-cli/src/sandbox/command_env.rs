//! Env secrets whose value is a host command's output (`source_command`).
//!
//! Resolved host-side just before a command runs, never inside the sandbox: the
//! command line comes from host-owned config, and only its trimmed stdout crosses
//! the boundary. Resolution is per command, behind a short cache, rather than once
//! at session start, because these are typically short-lived tokens
//! (`gcloud auth print-access-token` lasts an hour) and a session outlives them.
//!
//! The plan builder stays pure and never sees these: they are added to a built
//! plan by the paths that actually run something, so `cowboy sandbox plan` does
//! not run credential helpers to describe the boundary.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cowboy_core::config::SecurityConfig;

/// How long a resolved value is reused. Short enough that a token refreshed on
/// the host reaches the next command within a minute; long enough that a burst of
/// commands does not run `gcloud` (about a second each) before every one.
pub const TTL: Duration = Duration::from_secs(60);

/// Resolves and caches the `source_command` env secrets of one sandbox.
pub struct CommandEnv {
    /// `(env name, host command)`.
    secrets: Vec<(String, String)>,
    /// Where the commands run: the project root.
    cwd: PathBuf,
    cache: Mutex<Option<(Instant, Resolved)>>,
    /// Names whose failure was already reported, so a broken helper is mentioned
    /// once per session rather than before every command.
    reported: Mutex<BTreeSet<String>>,
}

/// One round of resolution: the values that came back, and the names that did not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    pub values: Vec<(String, String)>,
    pub failed: Vec<String>,
}

impl CommandEnv {
    pub fn new(security: &SecurityConfig, cwd: &Path) -> Self {
        let secrets = security
            .secrets
            .env
            .iter()
            .filter_map(|e| {
                let cmd = e.source_command.as_deref()?.trim();
                (!cmd.is_empty()).then(|| (e.name.clone(), cmd.to_string()))
            })
            .collect();
        Self {
            secrets,
            cwd: cwd.to_path_buf(),
            cache: Mutex::new(None),
            reported: Mutex::new(BTreeSet::new()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// The current values, running the host commands if the cache is stale.
    ///
    /// Blocking (each command may take up to its timeout); the commands of one
    /// round run concurrently, so a round costs the slowest of them, not the sum.
    pub fn resolve(&self) -> Resolved {
        self.resolve_with(Instant::now(), |cmd, cwd| {
            crate::project::run_value_command(cmd, Some(cwd))
        })
    }

    /// [`Self::resolve`] with the clock and the runner injected, for tests.
    pub fn resolve_with<F>(&self, now: Instant, run: F) -> Resolved
    where
        F: Fn(&str, &Path) -> Option<String> + Sync,
    {
        if let Some((at, r)) = self.cache.lock().expect("cache poisoned").as_ref() {
            if now.saturating_duration_since(*at) < TTL {
                return r.clone();
            }
        }
        let results: Vec<(String, Option<String>)> = std::thread::scope(|s| {
            let handles: Vec<_> = self
                .secrets
                .iter()
                .map(|(name, cmd)| {
                    let run = &run;
                    let cwd = self.cwd.as_path();
                    s.spawn(move || (name.clone(), run(cmd, cwd)))
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("secret resolver panicked"))
                .collect()
        });
        let mut r = Resolved::default();
        for (name, value) in results {
            match value {
                Some(v) => r.values.push((name, v)),
                None => r.failed.push(name),
            }
        }
        // Failures are cached too: a helper that times out would otherwise cost its
        // full timeout before every command.
        *self.cache.lock().expect("cache poisoned") = Some((now, r.clone()));
        r
    }

    /// Of `failed`, the names not reported before (and now marked reported).
    pub fn newly_failed(&self, failed: &[String]) -> Vec<String> {
        let mut seen = self.reported.lock().expect("reported poisoned");
        failed
            .iter()
            .filter(|n| seen.insert((*n).clone()))
            .cloned()
            .collect()
    }
}

/// Put resolved values into a plan's environment, replacing any same-named entry
/// so the command-sourced value is the one the sandbox sees.
pub fn apply(env: &mut Vec<(String, String)>, values: &[(String, String)]) {
    for (k, v) in values {
        env.retain(|(name, _)| name != k);
        env.push((k.clone(), v.clone()));
    }
    env.sort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::config::SecretEnv;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn security(secrets: &[(&str, &str)]) -> SecurityConfig {
        let mut sec = SecurityConfig::default();
        for (name, cmd) in secrets {
            sec.secrets.env.push(SecretEnv {
                name: name.to_string(),
                source_env: String::new(),
                source_command: Some(cmd.to_string()),
                required: false,
                approval: None,
            });
        }
        // A plain `source_env` secret is the plan's business, not this resolver's.
        sec.secrets.env.push(SecretEnv {
            name: "STATIC".into(),
            source_env: "SOME_HOST_VAR".into(),
            source_command: None,
            required: false,
            approval: None,
        });
        sec
    }

    #[test]
    fn values_are_cached_for_the_ttl_then_refreshed() {
        let env = CommandEnv::new(&security(&[("TOKEN", "print-token")]), Path::new("/p"));
        let calls = AtomicUsize::new(0);
        let run = |cmd: &str, cwd: &Path| {
            assert_eq!(cmd, "print-token");
            assert_eq!(cwd, Path::new("/p"), "runs in the project root");
            let n = calls.fetch_add(1, Ordering::SeqCst);
            Some(format!("tok-{n}"))
        };
        let t0 = Instant::now();
        let first = env.resolve_with(t0, run);
        assert_eq!(first.values, vec![("TOKEN".into(), "tok-0".into())]);
        let cached = env.resolve_with(t0 + TTL / 2, run);
        assert_eq!(cached, first, "inside the TTL the helper is not re-run");
        let fresh = env.resolve_with(t0 + TTL, run);
        assert_eq!(fresh.values, vec![("TOKEN".into(), "tok-1".into())]);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_failing_helper_is_reported_once_and_cached() {
        let env = CommandEnv::new(
            &security(&[("GOOD", "ok"), ("BAD", "broken")]),
            Path::new("/p"),
        );
        let calls = AtomicUsize::new(0);
        let run = |cmd: &str, _: &Path| {
            calls.fetch_add(1, Ordering::SeqCst);
            (cmd == "ok").then(|| "v".to_string())
        };
        let t0 = Instant::now();
        let r = env.resolve_with(t0, run);
        assert_eq!(r.values, vec![("GOOD".into(), "v".into())]);
        assert_eq!(r.failed, vec!["BAD".to_string()]);
        assert_eq!(env.newly_failed(&r.failed), vec!["BAD".to_string()]);
        assert!(env.newly_failed(&r.failed).is_empty(), "reported once");
        let _ = env.resolve_with(t0 + Duration::from_secs(1), run);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "the failure is cached too");
    }

    #[test]
    fn only_command_sourced_secrets_are_resolved() {
        let env = CommandEnv::new(&security(&[]), Path::new("/p"));
        assert!(env.is_empty());
        let r = env.resolve_with(Instant::now(), |_, _| panic!("nothing to run"));
        assert_eq!(r, Resolved::default());
    }

    #[test]
    fn apply_replaces_a_same_named_entry() {
        let mut env = vec![
            ("HOME".to_string(), "/home/agent".to_string()),
            ("TOKEN".to_string(), "stale".to_string()),
        ];
        apply(&mut env, &[("TOKEN".into(), "fresh".into())]);
        assert_eq!(
            env,
            vec![
                ("HOME".to_string(), "/home/agent".to_string()),
                ("TOKEN".to_string(), "fresh".to_string()),
            ]
        );
    }

    /// The real runner: a value is the helper's trimmed stdout, a failing or silent
    /// helper yields nothing, and the command runs in the project root.
    #[test]
    fn the_host_runner_trims_stdout_and_runs_in_the_project_root() {
        let dir = std::env::temp_dir();
        let sec = security(&[
            ("VAL", "printf '  tok-123\\n'"),
            ("FAIL", "echo nope; exit 3"),
            ("EMPTY", "true"),
            ("CWD", "pwd -P"),
        ]);
        let env = CommandEnv::new(&sec, &dir);
        let r = env.resolve();
        let get = |k: &str| {
            r.values
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("VAL").as_deref(), Some("tok-123"));
        assert_eq!(
            get("CWD").map(PathBuf::from),
            Some(std::fs::canonicalize(&dir).unwrap())
        );
        let mut failed = r.failed.clone();
        failed.sort();
        assert_eq!(failed, vec!["EMPTY".to_string(), "FAIL".to_string()]);
    }
}
