//! Resource limits, applied with an unprivileged cgroup v2.
//!
//! The plan has described memory, CPU and process ceilings since the sandbox was
//! first sketched; this is what makes them real. Without it a runaway build takes
//! the whole machine down, which under Docker the container runtime prevented for
//! free — so it is a capability that had to be rebuilt rather than one that was
//! merely nice to have.
//!
//! No privilege is needed. On a systemd host the user's own cgroup subtree is
//! **delegated**: `user@<uid>.service` already has `cpu memory pids` in its
//! `cgroup.subtree_control`, so a child cgroup can be created and configured by the
//! user who owns it. We find that delegated directory by walking up from our own
//! cgroup rather than hardcoding the systemd layout.
//!
//! Why cgroups and not `setrlimit`:
//!
//! - `RLIMIT_AS` bounds *address space*, not memory in use. Anything that reserves
//!   a large virtual mapping without touching it — a JVM, Go's runtime, ASan, a
//!   memory-mapped database — dies on a limit it never actually consumed.
//! - `RLIMIT_NPROC` is counted per-uid, not per-process-tree, so the sandbox's
//!   ceiling would be shared with (and consumed by) the user's own login session.
//! - Neither can bound CPU as a share of the machine.
//!
//! A cgroup bounds the process tree, counts what is actually used, and is torn down
//! with the session. If it cannot be created the limits are simply not enforced —
//! this is resource hygiene, not part of the security boundary, so it reports the
//! fact and continues rather than refusing to run.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use cowboy_sandbox::ResourceLimits;

/// The cgroup v2 mount point. Fixed by convention on every current distribution.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Controllers we need. A delegated directory that lacks any of them can still be
/// used for the others — a partial limit beats none.
const WANTED: &[&str] = &["memory", "cpu", "pids"];

/// A cgroup owning one session's processes.
#[derive(Debug)]
pub struct Cgroup {
    path: PathBuf,
    /// Which limits were actually written, for reporting. A controller can be
    /// delegated and still refuse a value, and silently claiming a ceiling that is
    /// not in force would be worse than saying nothing.
    applied: Vec<String>,
}

impl Cgroup {
    /// Create a cgroup for `name` and write `limits` into it.
    ///
    /// `Ok(None)` means no delegated cgroup subtree is available here (no cgroup v2,
    /// or a session whose subtree is not delegated). The caller should say so and
    /// carry on unlimited.
    pub fn create(name: &str, limits: &ResourceLimits) -> Result<Option<Self>> {
        if limits.memory_mib.is_none() && limits.cpus.is_none() && limits.pids.is_none() {
            return Ok(None); // nothing to enforce
        }
        // Try each candidate parent in turn, creating the real directory rather than
        // probing with a throwaway one first. An earlier version probed with a
        // directory named after the pid, which two sessions in the *same* process
        // raced on: the loser saw `EEXIST`, concluded cgroups were unavailable, and
        // ran unlimited while reporting limits were in force. Attempting the actual
        // thing has no such window and leaves nothing behind.
        for parent in candidate_parents() {
            let path = parent.join(format!("cowboy-{name}"));
            // Reuse an existing directory: a session restarting in the same process
            // should not fail because its previous cgroup was not reaped.
            match std::fs::create_dir(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {}
                Err(e) => {
                    tracing::debug!(path = %path.display(), error = %e, "cgroup parent unusable");
                    continue;
                }
            }
            let mut cg = Self {
                path,
                applied: Vec::new(),
            };
            cg.apply(limits);
            // A directory we cannot configure is worse than none: it would report
            // success while enforcing nothing. Reap it and try the next parent.
            if cg.applied.is_empty() {
                cg.remove();
                continue;
            }
            return Ok(Some(cg));
        }
        Ok(None)
    }

    fn apply(&mut self, limits: &ResourceLimits) {
        let available = self.controllers();
        let has = |c: &str| available.iter().any(|a| a == c);

        if let Some(mib) = limits.memory_mib {
            if has("memory") && self.write("memory.max", &(mib * 1024 * 1024).to_string()) {
                self.applied.push(format!("memory {mib} MiB"));
                // Also cap swap, or a memory ceiling just pushes the machine into
                // swapping instead of bounding the workload.
                let _ = self.write("memory.swap.max", "0");
            }
        }
        if let Some(cpus) = limits.cpus {
            // cgroup v2 states CPU as "quota period" in microseconds.
            const PERIOD_US: f64 = 100_000.0;
            let quota = (cpus * PERIOD_US).round().max(1000.0) as u64;
            if has("cpu") && self.write("cpu.max", &format!("{quota} {}", PERIOD_US as u64)) {
                self.applied.push(format!("cpu {cpus}"));
            }
        }
        if let Some(pids) = limits.pids {
            if has("pids") && self.write("pids.max", &pids.to_string()) {
                self.applied.push(format!("pids {pids}"));
            }
        }
    }

    /// Controllers this cgroup can actually use (what the parent delegated).
    fn controllers(&self) -> Vec<String> {
        read_list(&self.path.join("cgroup.controllers"))
    }

    fn write(&self, file: &str, value: &str) -> bool {
        let path = self.path.join(file);
        match std::fs::write(&path, value) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "could not set a resource limit");
                false
            }
        }
    }

    /// A human-readable summary of what is in force, or `None` if nothing is.
    pub fn summary(&self) -> Option<String> {
        (!self.applied.is_empty()).then(|| self.applied.join(", "))
    }

    /// Put `pid` in this cgroup. The process is bounded from that moment, so this is
    /// done before `exec` and the command never runs outside its limits.
    pub fn add_pid(&self, pid: u32) -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(self.path.join("cgroup.procs"))?;
        f.write_all(pid.to_string().as_bytes())
    }

    /// The `cgroup.procs` path, for a caller that must join between fork and exec
    /// (where allocating a `PathBuf` is best avoided).
    pub fn procs_path(&self) -> PathBuf {
        self.path.join("cgroup.procs")
    }

    /// Remove the cgroup. Only succeeds once it is empty, so call it after the
    /// session's processes are gone; best-effort either way.
    pub fn remove(&self) {
        if let Err(e) = std::fs::remove_dir(&self.path) {
            tracing::debug!(path = %self.path.display(), error = %e, "cgroup not reaped");
        }
    }
}

/// Directories where a cgroup with the controllers we need might be creatable,
/// nearest first.
///
/// Walks up from our own cgroup. Our own is skipped deliberately: cgroup v2 forbids
/// a cgroup from holding processes *and* enabling controllers for its children, and
/// ours holds us — so a child of it could never be given a controller. An ancestor
/// that already delegates the controllers (systemd's `user@<uid>.service` does) has
/// no such problem.
fn candidate_parents() -> Vec<PathBuf> {
    let Some(own) = own_cgroup() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ancestor in own.ancestors().skip(1) {
        if ancestor.as_os_str().is_empty() {
            break;
        }
        let dir = PathBuf::from(CGROUP_ROOT).join(ancestor.strip_prefix("/").unwrap_or(ancestor));
        if !dir.is_dir() {
            continue;
        }
        // The controllers a child would get are the parent's *subtree_control*, not
        // its `controllers` — the latter is what the parent itself may use.
        let delegated = read_list(&dir.join("cgroup.subtree_control"));
        if WANTED.iter().any(|w| delegated.iter().any(|d| d == w)) {
            out.push(dir);
        }
    }
    out
}

/// Our cgroup path as it appears in `/proc/self/cgroup` (the v2 line, `0::`).
fn own_cgroup() -> Option<PathBuf> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    parse_own_cgroup(&text)
}

/// Split out so the `/proc` format is testable without a particular host layout.
fn parse_own_cgroup(text: &str) -> Option<PathBuf> {
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| PathBuf::from(p.trim()))
}

fn read_list(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Whether this host can enforce limits at all, for `cowboy doctor` to report
/// without creating anything.
pub fn available() -> bool {
    !candidate_parents().is_empty()
}

/// Remove leftover cowboy cgroup directories that no longer hold any process.
///
/// A clean shutdown reaps its own, but a crashed worker cannot: the holder dies with
/// it, which empties the cgroup, and an empty cgroup then persists until someone
/// removes the directory. Returns how many were reaped.
///
/// Only ever removes a directory that is **empty of processes**, so this cannot
/// disturb a live session — including one belonging to another project.
pub fn reap_empty() -> usize {
    let mut reaped = 0;
    for parent in candidate_parents() {
        let Ok(entries) = std::fs::read_dir(&parent) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_ours = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("cowboy-"));
            if !is_ours || !path.is_dir() {
                continue;
            }
            // `cgroup.procs` empty means no member processes. `remove_dir` would fail
            // anyway if it were occupied, but checking first keeps the count honest.
            let occupied = std::fs::read_to_string(path.join("cgroup.procs"))
                .map(|s| !s.trim().is_empty())
                .unwrap_or(true);
            if !occupied && std::fs::remove_dir(&path).is_ok() {
                reaped += 1;
            }
        }
    }
    reaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_v2_cgroup_line_is_parsed() {
        let text = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/x.scope\n";
        assert_eq!(
            parse_own_cgroup(text),
            Some(PathBuf::from(
                "/user.slice/user-1000.slice/user@1000.service/app.slice/x.scope"
            ))
        );
    }

    /// A cgroup v1 host has numbered controller lines and no `0::` line. It must
    /// yield nothing rather than a nonsense path.
    #[test]
    fn a_v1_only_host_yields_no_cgroup() {
        let text = "3:memory:/user/1000.user\n2:cpu,cpuacct:/user\n1:name=systemd:/user\n";
        assert_eq!(parse_own_cgroup(text), None);
    }

    #[test]
    fn a_missing_or_empty_file_yields_nothing() {
        assert_eq!(parse_own_cgroup(""), None);
        assert_eq!(
            read_list(Path::new("/nonexistent/cgroup.controllers")),
            Vec::<String>::new()
        );
    }

    /// No limits configured means no cgroup: creating an empty one would leave
    /// directories behind for nothing.
    #[test]
    fn no_limits_means_no_cgroup() {
        let none = ResourceLimits::default();
        assert!(Cgroup::create("test-none", &none).unwrap().is_none());
    }

    /// The real thing, where the host allows it. Asserts the values landed in the
    /// files the kernel reads, which is the only evidence that a limit is in force.
    #[test]
    fn limits_are_written_where_the_kernel_reads_them() {
        if !available() {
            eprintln!("skipping: no delegated cgroup v2 subtree here");
            return;
        }
        let limits = ResourceLimits {
            memory_mib: Some(512),
            cpus: Some(2.0),
            pids: Some(128),
            jobs: Some(2),
        };
        let cg = Cgroup::create(&format!("test-{}", std::process::id()), &limits)
            .unwrap()
            .expect("a cgroup should have been created");

        let read = |f: &str| std::fs::read_to_string(cg.path.join(f)).unwrap_or_default();
        assert_eq!(read("memory.max").trim(), (512 * 1024 * 1024).to_string());
        assert_eq!(read("cpu.max").trim(), "200000 100000");
        assert_eq!(read("pids.max").trim(), "128");
        assert!(
            cg.summary().is_some(),
            "what is in force must be reportable"
        );

        cg.remove();
        assert!(!cg.path.exists(), "an empty cgroup must be reaped");
    }

    /// A fractional CPU quota must not round to zero, which the kernel rejects.
    #[test]
    fn a_tiny_cpu_quota_stays_valid() {
        if !available() {
            eprintln!("skipping: no delegated cgroup v2 subtree here");
            return;
        }
        let limits = ResourceLimits {
            cpus: Some(0.001),
            ..Default::default()
        };
        let cg = Cgroup::create(&format!("tiny-{}", std::process::id()), &limits)
            .unwrap()
            .expect("a cgroup");
        let quota = std::fs::read_to_string(cg.path.join("cpu.max")).unwrap_or_default();
        assert!(
            quota.split_whitespace().next().unwrap_or("0") != "0",
            "a zero quota is rejected by the kernel: {quota}"
        );
        cg.remove();
    }

    /// Regression: concurrent sessions in one process must each get a working cgroup.
    ///
    /// An earlier version checked whether a parent was usable by creating a probe
    /// directory named after the pid. Two sessions in the same process therefore
    /// raced on one name; the loser saw `EEXIST`, concluded no cgroup subtree was
    /// available, and ran **unlimited while reporting that limits were in force**.
    /// That is the worst possible failure for this feature, and it only showed up as
    /// an intermittently failing end-to-end test.
    #[test]
    fn concurrent_sessions_each_get_enforced_limits() {
        if !available() {
            eprintln!("skipping: no delegated cgroup v2 subtree here");
            return;
        }
        let limits = ResourceLimits {
            memory_mib: Some(256),
            pids: Some(64),
            ..Default::default()
        };
        let created: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let limits = limits.clone();
                    s.spawn(move || {
                        Cgroup::create(&format!("race-{}-{i}", std::process::id()), &limits)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("no panic").expect("no error"))
                .collect()
        });

        assert_eq!(
            created.iter().filter(|c| c.is_some()).count(),
            8,
            "every concurrent session must get a cgroup, not just the first"
        );
        for cg in created.into_iter().flatten() {
            assert_eq!(
                std::fs::read_to_string(cg.path.join("memory.max"))
                    .unwrap_or_default()
                    .trim(),
                (256 * 1024 * 1024).to_string(),
                "and each must actually be configured"
            );
            cg.remove();
        }
    }

    /// A cgroup that cannot be configured must not be reported as enforcing
    /// something. `summary()` is what the user is told, so it may only name limits
    /// that were really written.
    #[test]
    fn nothing_is_reported_as_in_force_unless_it_was_written() {
        let cg = Cgroup {
            path: PathBuf::from("/nonexistent/cgroup"),
            applied: Vec::new(),
        };
        assert!(cg.summary().is_none());
    }
}
