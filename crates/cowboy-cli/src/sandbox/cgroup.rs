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
            // Reuse an existing directory: a session whose holder died restarts with
            // the same name, and should not fail because its previous cgroup was not
            // reaped. Names are per-instance (`project::cgroup_key`), so the directory
            // being reused is always this sandbox's own.
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

/// Whether this host can enforce limits at all, for `cowboy doctor` to report and for
/// tests to skip on.
///
/// Verified by **doing it**: create a real cgroup, confirm a limit landed, remove it.
/// It used to just ask whether any ancestor delegated the controllers, which is a
/// weaker claim than it looks — a directory can advertise `memory cpu pids` in its
/// `cgroup.subtree_control` and still refuse `mkdir` to the current user. A GitHub
/// Actions runner is exactly that shape, and the results were:
///
/// - `cowboy doctor` printed `resource limits: cgroup v2 subtree delegated` on a host
///   where no ceiling would ever apply, and `cowboy sandbox plan` omitted its
///   `NOT ENFORCED` warning — the module's own rule about never claiming a limit that
///   is not in force, broken by the check meant to report it;
/// - every test guarded on this ran instead of skipping, then failed inside
///   `Cgroup::create`. The guard said yes and the thing it guarded said no.
///
/// Implemented by calling `Cgroup::create` rather than re-deriving its conditions, so
/// the answer cannot drift from what actually happens. All three controllers are
/// requested because `create` succeeds if *any* limit applied, and a host that
/// delegates only `cpu` can still enforce something worth reporting.
pub fn available() -> bool {
    let probe = ResourceLimits {
        memory_mib: Some(64),
        cpus: Some(1.0),
        pids: Some(16),
        jobs: None,
    };
    // Unique per call: concurrent probes (the test suite runs in parallel) must not
    // race on one name and conclude the host cannot do this — the bug this module
    // already fixed once inside `create`.
    match Cgroup::create(&probe_name(), &probe) {
        Ok(Some(cg)) => {
            cg.remove();
            true
        }
        _ => false,
    }
}

/// A cgroup name no other probe or session will pick.
fn probe_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "probe-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
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

    /// Whether to skip a test that needs a real cgroup.
    ///
    /// Deliberately **not** gated on `COWBOY_SANDBOX_TESTS=required`. That switch means
    /// "the security boundary must work here", and resource limits are explicitly not
    /// part of it — they protect the machine from a runaway build, and the sandbox
    /// confines correctly without them. A CI runner has no delegated subtree and should
    /// still be able to demand a working boundary.
    ///
    /// `COWBOY_CGROUP_TESTS=required` is the narrower switch, for a host that does have
    /// delegation (a systemd user session) and wants to know if it silently loses it.
    fn skip_no_cgroups() -> bool {
        if available() {
            return false;
        }
        assert!(
            std::env::var("COWBOY_CGROUP_TESTS").as_deref() != Ok("required"),
            "COWBOY_CGROUP_TESTS=required but no usable cgroup v2 subtree here — the \
             delegated parent may advertise controllers while refusing mkdir to this user"
        );
        eprintln!("skipping: no usable cgroup v2 subtree here");
        true
    }

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
        if skip_no_cgroups() {
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
        if skip_no_cgroups() {
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
        if skip_no_cgroups() {
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

    /// The skip guard must agree with the thing it guards.
    ///
    /// `available()` used to answer "does an ancestor delegate the controllers?", which
    /// is a weaker claim than "can a cgroup be created here" — a directory can advertise
    /// `memory cpu pids` and still refuse `mkdir` to this user. On such a host (a
    /// GitHub Actions runner) every guarded test ran instead of skipping and then failed
    /// inside `create`, and `doctor` reported limits as available on a machine where
    /// none would apply. Whatever this host is, the two must not disagree.
    #[test]
    fn the_availability_check_agrees_with_actually_creating_one() {
        let limits = ResourceLimits {
            memory_mib: Some(128),
            ..Default::default()
        };
        let created = Cgroup::create(&format!("agree-{}", std::process::id()), &limits).unwrap();
        assert_eq!(
            available(),
            created.is_some(),
            "the guard and the operation must reach the same conclusion on this host"
        );
        if let Some(cg) = created {
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
