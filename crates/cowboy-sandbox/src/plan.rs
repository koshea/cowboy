//! [`SandboxPlan`]: the complete description of one command's confinement.
//!
//! A pure function of host-owned config, the project root, and the current grant
//! set. Built fresh for **every command**, which is what makes runtime grants
//! possible at all: a path approved a moment ago is simply an entry in the next
//! plan, with no session restart and nothing to reconfigure. (Docker could not do
//! this — a container's mounts are fixed when it is created.)

use std::path::{Path, PathBuf};

use cowboy_core::config::{self, SecurityConfig};
use cowboy_core::error::{Error, Result};

use crate::denylist::Denylist;
use crate::probe::HostProbe;

/// How a path is exposed inside the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BindMode {
    ReadOnly,
    ReadWrite,
}

/// One path exposed inside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    pub source: PathBuf,
    pub target: String,
    pub mode: BindMode,
    /// Why this bind exists, for `cowboy sandbox plan` and for reviewing a diff of
    /// the boundary rather than a list of paths.
    pub why: String,
}

impl Bind {
    fn ro(source: impl Into<PathBuf>, target: impl Into<String>, why: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            mode: BindMode::ReadOnly,
            why: why.into(),
        }
    }
    fn rw(source: impl Into<PathBuf>, target: impl Into<String>, why: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            mode: BindMode::ReadWrite,
            why: why.into(),
        }
    }
}

/// The Landlock domain applied immediately before exec.
///
/// Defence in depth over the mount view, not a replacement for it: Landlock is
/// enforced by the kernel against the *process*, so it still holds if a bind is
/// wrong, survives into every descendant, and can only ever be narrowed further.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LandlockRules {
    pub read_only: Vec<PathBuf>,
    pub read_write: Vec<PathBuf>,
    /// TCP ports the sandbox may `connect()` to. Landlock gates ports, not
    /// addresses — which is enough here only because the sandbox network namespace
    /// can reach nothing but its own loopback anyway.
    pub connect_tcp: Vec<u16>,
    /// Scope the domain against signalling and abstract-socket-connecting outside
    /// it (Landlock ABI 6). Hardening only: no trust boundary depends on it.
    pub scope_ipc: bool,
}

/// The seccomp filter applied immediately before exec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeccompProfile {
    /// Syscalls refused outright.
    pub denied: Vec<&'static str>,
    /// Refuse `socket(AF_INET|AF_INET6, SOCK_RAW|SOCK_DGRAM_non_dns, …)`.
    pub deny_raw_sockets: bool,
}

impl Default for SeccompProfile {
    fn default() -> Self {
        Self {
            denied: vec![
                // io_uring submits operations as ring entries, NOT syscalls, so
                // IORING_OP_CONNECT / IORING_OP_OPENAT would sail straight past a
                // filter on connect/openat. Landlock's LSM hooks do cover io_uring
                // for filesystem access, so file confinement holds either way —
                // but the seccomp half is bypassable unless the ring is refused
                // outright. Denying it costs the agent nothing we care about.
                "io_uring_setup",
                "io_uring_enter",
                "io_uring_register",
                // Kernel module and kexec surface: never legitimate from a build.
                "init_module",
                "finit_module",
                "delete_module",
                "kexec_load",
                "kexec_file_load",
                // Tracing and BPF: escape and inspection primitives.
                "bpf",
                "perf_event_open",
                // Privileged host-wide operations.
                "pivot_root",
                "swapon",
                "swapoff",
                "reboot",
                "settimeofday",
                "clock_settime",
                "clock_adjtime",
                "adjtimex",
                // Legacy / rarely-used interfaces with a poor security record.
                "uselib",
                "userfaultfd",
                "personality",
                "ptrace",
                "process_vm_readv",
                "process_vm_writev",
            ],
            deny_raw_sockets: true,
        }
    }
}

/// Resource bounds for the sandbox.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResourceLimits {
    /// Memory ceiling in MiB, `None` for unlimited.
    pub memory_mib: Option<u64>,
    /// CPU quota in cores, `None` for unlimited.
    pub cpus: Option<f64>,
    /// Process ceiling, bounding a fork bomb.
    pub pids: Option<u32>,
    /// Build parallelism derived from `cpus`, injected as `MAKEFLAGS` and friends.
    ///
    /// A cgroup CPU quota does not change what `nproc` reports, so without this a
    /// build sizes itself from the host's core count and can OOM the box.
    pub jobs: Option<u32>,
}

/// Everything needed to confine and run one command.
#[derive(Debug, Clone)]
pub struct SandboxPlan {
    pub binds: Vec<Bind>,
    /// Mount a fresh `procfs` here; a private PID namespace makes it show only
    /// the sandbox's own processes — which is also what hides the relay.
    pub proc_at: String,
    /// Minimal device set (`null`, `zero`, `urandom`, tty…), never the host `/dev`.
    pub dev_at: String,
    pub tmpfs: Vec<String>,
    pub symlinks: Vec<(String, String)>,
    pub env: Vec<(String, String)>,
    pub workdir: String,
    pub landlock: LandlockRules,
    pub seccomp: SeccompProfile,
    pub limits: ResourceLimits,
}

/// Where the `cowboy` binary is bound inside the sandbox, for bwrap to exec as
/// the lockdown shim.
///
/// A fixed top-level path because it must not be shadowed: `/run` and `/tmp` are
/// tmpfs mounted *after* the binds (so nothing can shadow them), and `/usr` is a
/// read-only bind of the host's, so a mount point cannot be created inside it. The
/// leading dot keeps it out of the way of anything a project might use.
pub const SHIM_PATH: &str = "/.cowboy-shim";

/// Host directories exposed read-only so the agent can use the machine's own
/// toolchain. This is the flexibility the Docker image could not offer: the agent
/// gets the compilers, language runtimes and CLIs the user actually has, at the
/// versions they actually installed, with nothing to build or pull.
const HOST_TOOLCHAIN_DIRS: &[&str] = &["/usr", "/opt"];

/// Symlinks recreating a merged-`/usr` layout, so `/bin/sh` and `/lib64/ld.so`
/// resolve after `pivot_root` onto a fresh root.
const USR_SYMLINKS: &[(&str, &str)] = &[
    ("usr/bin", "/bin"),
    ("usr/sbin", "/sbin"),
    ("usr/lib", "/lib"),
    ("usr/lib64", "/lib64"),
];

/// Config files from `/etc` the toolchain genuinely needs. An allowlist rather
/// than the whole directory: `/etc` holds shadow, ssh host keys, and every
/// service credential on the box.
const ETC_ALLOW: &[&str] = &[
    "/etc/alternatives",
    "/etc/ca-certificates",
    "/etc/ca-certificates.conf",
    "/etc/ssl",
    "/etc/pki",
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/localtime",
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/nsswitch.conf",
    "/etc/terminfo",
    "/etc/profile.d",
    "/etc/gitconfig",
    "/etc/env.d",
];

/// A path granted at runtime, after host-side approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub path: PathBuf,
    pub read_only: bool,
}

/// What the plan is being built from.
pub struct PlanInputs<'a> {
    /// Canonicalized project root, bound at the sandbox workdir.
    pub root: &'a Path,
    pub security: &'a SecurityConfig,
    /// Paths approved at runtime this session (see the `request_path` tool).
    pub grants: &'a [Grant],
    /// An empty, read-only file bound over host-owned config to mask it.
    pub mask_file: &'a Path,
    /// Loopback port of the egress relay, the only TCP destination permitted.
    pub relay_port: u16,
}

impl SandboxPlan {
    /// Build the plan, or fail if configuration or a grant would breach the
    /// boundary.
    ///
    /// Ordering matters and is deliberate: host toolchain first, then the project,
    /// then credential grants, then runtime grants, and the host-owned config mask
    /// **last** — so no later entry can re-expose what the mask hid.
    pub fn build(inputs: &PlanInputs<'_>, probe: &dyn HostProbe) -> Result<Self> {
        let sec = inputs.security;
        let workdir = sec.container.workdir.clone();
        let denylist = Denylist::build(probe, inputs.root);
        let mut binds = Vec::new();

        // 0. The lockdown shim: the cowboy binary itself, read-only. bwrap cannot
        //    apply Landlock, so it execs this instead of the command directly, and
        //    it must therefore be reachable inside the sandbox. Read-only, and the
        //    denylist separately prevents any runtime grant making it writable.
        if let Some(exe) = probe.self_exe() {
            binds.push(Bind::ro(exe, SHIM_PATH, "lockdown shim (cowboy binary)"));
        }

        // 1. The host's own toolchain, read-only.
        for dir in HOST_TOOLCHAIN_DIRS {
            let p = PathBuf::from(dir);
            if probe.exists(&p) {
                binds.push(Bind::ro(p, *dir, "host toolchain"));
            }
        }
        for entry in ETC_ALLOW {
            let p = PathBuf::from(entry);
            if probe.exists(&p) {
                binds.push(Bind::ro(p, *entry, "toolchain configuration"));
            }
        }

        // 2. The project and any other configured mounts. The default config
        //    mounts `.` at the workdir, so the project arrives through here rather
        //    than being hardcoded — one source of truth, and no second bind that
        //    could silently downgrade the project to read-only by landing later.
        let mut mounts_workdir = false;
        for m in &sec.container.mounts {
            let source = resolve_source(inputs.root, &m.source);
            // The same invariant `SecurityConfig::validate` enforces, re-checked
            // here because a mount can also arrive via the user's personal
            // overlay, which is merged after that validation runs.
            if let Some(reason) = denylist.check(&source) {
                return Err(Error::SecurityInvariant(format!(
                    "mount {} is refused: {}",
                    source.display(),
                    reason.explain()
                )));
            }
            let mode = if m.mode == "ro" {
                BindMode::ReadOnly
            } else {
                BindMode::ReadWrite
            };
            if m.target == workdir {
                mounts_workdir = true;
            }
            let why = if source == inputs.root {
                "the project"
            } else {
                "configured mount"
            };
            binds.push(Bind {
                source,
                target: m.target.clone(),
                mode,
                why: why.into(),
            });
        }
        // An agent with no project is never what anyone meant; say so rather than
        // starting a session whose workdir does not exist.
        if !mounts_workdir {
            return Err(Error::Invalid(format!(
                "no mount targets the workdir {workdir}; the agent would have no project. \
                 Add a mount with target: {workdir} to .cowboy/security.yaml."
            )));
        }

        // 3. A linked worktree's shared git dir, at its own host path so the
        //    absolute gitdir reference in `.git` resolves. Writable so the
        //    worktree's branch can write objects and refs.
        if let Some(common) = probe.git_common_dir(inputs.root) {
            let t = common.to_string_lossy().into_owned();
            binds.push(Bind::rw(common, t, "shared git dir (linked worktree)"));
        }

        // 4. Credential grants from host-owned config. Deliberate and out-of-band:
        //    these come from a file the user edited, which is exactly the gate the
        //    runtime-grant denylist preserves.
        for grant in &sec.secrets.files {
            let Some(source) = probe.expand(&grant.source) else {
                continue;
            };
            if !probe.exists(&source) {
                if grant.required {
                    return Err(Error::Invalid(format!(
                        "required credential {} is missing on the host",
                        source.display()
                    )));
                }
                continue;
            }
            binds.push(Bind {
                source,
                target: grant.target.clone(),
                mode: if grant.read_only {
                    BindMode::ReadOnly
                } else {
                    BindMode::ReadWrite
                },
                why: "credential grant (security.yaml)".into(),
            });
        }

        // 5. Runtime grants. Re-checked against the denylist here as well as at
        //    approval time: this is the load-bearing check, since it is the one a
        //    persisted or hand-edited grant must also pass.
        for g in inputs.grants {
            if let Some(reason) = denylist.check(&g.path) {
                return Err(Error::SecurityInvariant(format!(
                    "granted path {} is refused: {}",
                    g.path.display(),
                    reason.explain()
                )));
            }
            let target = g.path.to_string_lossy().into_owned();
            binds.push(Bind {
                source: g.path.clone(),
                target,
                mode: if g.read_only {
                    BindMode::ReadOnly
                } else {
                    BindMode::ReadWrite
                },
                why: "runtime grant (approved by the user)".into(),
            });
        }

        // 6. Mask host-owned config LAST. It lives under the project directory, so
        //    it is inside a bind the agent can otherwise read; an empty read-only
        //    file over it means the agent cannot learn its own boundary.
        for file in [config::SECURITY_FILE, config::MODELS_FILE] {
            let host_path = inputs.root.join(config::COWBOY_DIR).join(file);
            if probe.exists(&host_path) {
                binds.push(Bind::ro(
                    inputs.mask_file.to_path_buf(),
                    format!("{workdir}/{}/{file}", config::COWBOY_DIR),
                    "mask host-owned config",
                ));
            }
        }

        let limits = resolve_limits(sec);
        let env = build_env(sec, &workdir, &limits);
        let landlock = landlock_for(&binds, inputs.relay_port);

        Ok(Self {
            binds,
            proc_at: "/proc".into(),
            dev_at: "/dev".into(),
            tmpfs: vec!["/tmp".into(), "/run".into(), "/var/tmp".into()],
            symlinks: USR_SYMLINKS
                .iter()
                .map(|(t, l)| (t.to_string(), l.to_string()))
                .collect(),
            env,
            workdir,
            landlock,
            seccomp: SeccompProfile::default(),
            limits,
        })
    }

    /// A human-readable rendering, for `cowboy sandbox plan`.
    ///
    /// The boundary should be inspectable without reading the source or trusting
    /// a summary — this is what the user checks when they want to know what the
    /// agent can actually reach.
    pub fn render(&self, denylist: &Denylist) -> String {
        let mut s = String::new();
        s.push_str("filesystem\n");
        for b in &self.binds {
            let mode = match b.mode {
                BindMode::ReadOnly => "ro",
                BindMode::ReadWrite => "rw",
            };
            s.push_str(&format!(
                "  {mode}  {} -> {}   ({})\n",
                b.source.display(),
                b.target,
                b.why
            ));
        }
        s.push_str(&format!("  tmpfs {}\n", self.tmpfs.join(", ")));
        s.push_str(&format!("  proc {}   dev {}\n", self.proc_at, self.dev_at));

        s.push_str("\nlandlock\n");
        s.push_str(&format!(
            "  {} read-only, {} read-write paths\n",
            self.landlock.read_only.len(),
            self.landlock.read_write.len()
        ));
        s.push_str(&format!(
            "  tcp connect allowed: {}\n",
            self.landlock
                .connect_tcp
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        s.push_str(&format!("  ipc scoping: {}\n", self.landlock.scope_ipc));

        s.push_str("\nseccomp\n");
        s.push_str(&format!(
            "  {} syscalls denied (incl. io_uring_setup), raw sockets denied: {}\n",
            self.seccomp.denied.len(),
            self.seccomp.deny_raw_sockets
        ));

        s.push_str("\nlimits\n");
        s.push_str(&format!(
            "  memory {:?} MiB, cpus {:?}, pids {:?}, build jobs {:?}\n",
            self.limits.memory_mib, self.limits.cpus, self.limits.pids, self.limits.jobs
        ));

        s.push_str(&format!(
            "\nnever grantable at any approval scope ({} paths)\n",
            denylist.len()
        ));
        for p in denylist.paths() {
            s.push_str(&format!("  {}\n", p.display()));
        }
        s.push_str("  (plus .cowboy/, security.yaml, models.yaml, providers.yaml anywhere)\n");
        s
    }
}

/// Landlock rules mirroring the bind list.
///
/// Derived from the binds rather than written separately, so the two cannot
/// disagree about what is writable.
fn landlock_for(binds: &[Bind], relay_port: u16) -> LandlockRules {
    let mut read_only = Vec::new();
    let mut read_write = Vec::new();
    for b in binds {
        match b.mode {
            BindMode::ReadOnly => read_only.push(b.source.clone()),
            BindMode::ReadWrite => read_write.push(b.source.clone()),
        }
    }
    // Writable scratch. `/tmp` is a fresh tmpfs per command, not the host's.
    read_write.push(PathBuf::from("/tmp"));
    LandlockRules {
        read_only,
        read_write,
        connect_tcp: vec![relay_port],
        scope_ipc: true,
    }
}

/// Resolve `auto` limits and the build parallelism that follows from the CPU quota.
fn resolve_limits(sec: &SecurityConfig) -> ResourceLimits {
    let cpus = sec.container.cpus.as_ref().map(|c| match c {
        config::CpuLimit::Auto => config::auto_cpus(num_cpus()),
        config::CpuLimit::Cores(n) => *n,
    });
    let memory_mib = sec.container.memory.as_deref().and_then(|m| {
        if m.eq_ignore_ascii_case("auto") {
            Some(config::auto_mem_mib(host_mem_mib()))
        } else {
            parse_mem_mib(m)
        }
    });
    ResourceLimits {
        memory_mib,
        cpus,
        pids: Some(4096),
        jobs: cpus.map(|c| (c.max(1.0)) as u32),
    }
}

/// Environment for the command. No `HOME=/tmp` workaround is needed any more:
/// under Docker the agent ran as a uid with no passwd entry, so `HOME` had to
/// point somewhere world-writable. Here it gets an ordinary, confined home.
fn build_env(
    sec: &SecurityConfig,
    workdir: &str,
    limits: &ResourceLimits,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("HOME".to_string(), format!("{workdir}/.cowboy/home")),
        ("COWBOY_SANDBOX".to_string(), "1".to_string()),
    ];
    if let Some(j) = limits.jobs {
        let j = j.to_string();
        for k in [
            "MAKEFLAGS",
            "MAKE_OPTS",
            "CARGO_BUILD_JOBS",
            "npm_config_jobs",
            "CMAKE_BUILD_PARALLEL_LEVEL",
            "MISE_JOBS",
        ] {
            let v = if k.starts_with("MAKE") {
                format!("-j{j}")
            } else {
                j.clone()
            };
            env.push((k.to_string(), v));
        }
    }
    // Static secrets sourced from host env vars. `source_command` secrets are not
    // here on purpose — they are resolved fresh per command so short-lived tokens
    // refresh mid-session. Values are never logged.
    for s in &sec.secrets.env {
        if s.source_command.is_some() || s.source_env.is_empty() {
            continue;
        }
        if let Ok(v) = std::env::var(&s.source_env) {
            env.push((s.name.clone(), v));
        }
    }
    env.sort();
    env
}

/// Resolve a configured mount source: `.` means the project root, a relative path
/// is relative to it, and an absolute path is taken as-is.
fn resolve_source(root: &Path, source: &str) -> PathBuf {
    if source == "." {
        return root.to_path_buf();
    }
    let p = Path::new(source);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

fn parse_mem_mib(raw: &str) -> Option<u64> {
    let s = raw.trim().to_ascii_lowercase();
    let (num, mult) = match s.strip_suffix('g') {
        Some(n) => (n, 1024),
        None => match s.strip_suffix('m') {
            Some(n) => (n, 1),
            None => (s.as_str(), 1),
        },
    };
    num.trim().parse::<u64>().ok().map(|n| n * mult)
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Total host memory in MiB from `/proc/meminfo`, 0 when unreadable (which makes
/// `auto` clamp to its floor rather than guess high).
fn host_mem_mib() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                .map(|kb| kb / 1024)
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::FakeHost;
    use cowboy_core::config::{Mount, SecretMount};

    fn inputs<'a>(
        root: &'a Path,
        security: &'a SecurityConfig,
        grants: &'a [Grant],
        mask: &'a Path,
    ) -> PlanInputs<'a> {
        PlanInputs {
            root,
            security,
            grants,
            mask_file: mask,
            relay_port: 8443,
        }
    }

    fn host() -> FakeHost {
        FakeHost::new().with_existing([
            "/usr",
            "/etc/ssl",
            "/etc/resolv.conf",
            "/srv/proj",
            "/srv/proj/.cowboy/security.yaml",
            "/srv/proj/.cowboy/models.yaml",
        ])
    }

    fn plan_with(
        security: &SecurityConfig,
        grants: &[Grant],
        probe: &dyn HostProbe,
    ) -> Result<SandboxPlan> {
        let root = Path::new("/srv/proj");
        let mask = Path::new("/run/cowboy/mask");
        SandboxPlan::build(&inputs(root, security, grants, mask), probe)
    }

    /// The invariant with a dedicated E2E test under Docker, preserved here.
    #[test]
    fn masks_host_owned_config() {
        let sec = SecurityConfig::default();
        let plan = plan_with(&sec, &[], &host()).unwrap();
        let workdir = &sec.container.workdir;
        for f in ["security.yaml", "models.yaml"] {
            let target = format!("{workdir}/.cowboy/{f}");
            let bind = plan
                .binds
                .iter()
                .find(|b| b.target == target)
                .unwrap_or_else(|| panic!("no mask bind for {f}"));
            assert_eq!(bind.source, Path::new("/run/cowboy/mask"));
            assert_eq!(bind.mode, BindMode::ReadOnly);
        }
    }

    /// The mask must be applied after everything else, or a later bind could
    /// re-expose the file it hid.
    #[test]
    fn mask_binds_come_last() {
        let sec = SecurityConfig::default();
        let plan = plan_with(&sec, &[], &host()).unwrap();
        let first_mask = plan
            .binds
            .iter()
            .position(|b| b.why == "mask host-owned config")
            .unwrap();
        assert!(
            plan.binds[first_mask..]
                .iter()
                .all(|b| b.why == "mask host-owned config"),
            "a non-mask bind follows the mask and could re-expose host-owned config"
        );
    }

    #[test]
    fn masks_only_files_that_exist() {
        let probe = FakeHost::new().with_existing(["/usr", "/srv/proj"]);
        let sec = SecurityConfig::default();
        let plan = plan_with(&sec, &[], &probe).unwrap();
        assert!(!plan.binds.iter().any(|b| b.why == "mask host-owned config"));
    }

    #[test]
    fn binds_the_shared_git_dir_for_a_linked_worktree() {
        let probe = host().as_linked_worktree("/srv/main/.git/worktrees/wt");
        let sec = SecurityConfig::default();
        let plan = plan_with(&sec, &[], &probe).unwrap();
        let b = plan
            .binds
            .iter()
            .find(|b| b.why.contains("linked worktree"))
            .expect("linked worktree needs its shared git dir bound");
        // Same path inside as outside: `.git` holds an absolute gitdir reference.
        assert_eq!(b.source, Path::new("/srv/main/.git/worktrees/wt"));
        assert_eq!(b.target, "/srv/main/.git/worktrees/wt");
        assert_eq!(
            b.mode,
            BindMode::ReadWrite,
            "git must write objects and refs"
        );
    }

    #[test]
    fn no_git_bind_for_an_ordinary_checkout() {
        let plan = plan_with(&SecurityConfig::default(), &[], &host()).unwrap();
        assert!(!plan.binds.iter().any(|b| b.why.contains("linked worktree")));
    }

    #[test]
    fn exposes_the_host_toolchain_read_only() {
        let plan = plan_with(&SecurityConfig::default(), &[], &host()).unwrap();
        let usr = plan
            .binds
            .iter()
            .find(|b| b.target == "/usr")
            .expect("the host toolchain is the point of the rewrite");
        assert_eq!(usr.mode, BindMode::ReadOnly);
    }

    /// `/etc` is an allowlist: it holds shadow, ssh host keys, and service creds.
    #[test]
    fn does_not_bind_all_of_etc() {
        let probe = host().with_existing(["/etc", "/etc/shadow"]);
        let plan = plan_with(&SecurityConfig::default(), &[], &probe).unwrap();
        assert!(!plan.binds.iter().any(|b| b.target == "/etc"));
        assert!(!plan.binds.iter().any(|b| b.target == "/etc/shadow"));
    }

    #[test]
    fn the_project_is_writable() {
        let plan = plan_with(&SecurityConfig::default(), &[], &host()).unwrap();
        let sec = SecurityConfig::default();
        let b = plan
            .binds
            .iter()
            .find(|b| b.target == sec.container.workdir)
            .unwrap();
        assert_eq!(b.source, Path::new("/srv/proj"));
        assert_eq!(b.mode, BindMode::ReadWrite);
    }

    /// The project must be bound exactly once. Two binds for the same target
    /// would let the later one silently downgrade the project to read-only.
    #[test]
    fn the_project_is_bound_exactly_once() {
        let sec = SecurityConfig::default();
        let plan = plan_with(&sec, &[], &host()).unwrap();
        let n = plan
            .binds
            .iter()
            .filter(|b| b.target == sec.container.workdir)
            .count();
        assert_eq!(n, 1, "duplicate workdir binds: {:#?}", plan.binds);
    }

    /// A config with nothing at the workdir is a mistake worth naming, not a
    /// session with an empty project directory.
    #[test]
    fn refuses_config_with_no_mount_at_the_workdir() {
        let mut sec = SecurityConfig::default();
        sec.container.mounts.clear();
        let err = plan_with(&sec, &[], &host()).expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("no mount targets the workdir"), "{msg}");
        assert!(
            msg.contains("/workspace"),
            "the message should name the workdir: {msg}"
        );
    }

    #[test]
    fn runtime_grants_become_binds() {
        let grants = [Grant {
            path: PathBuf::from("/srv/other-repo"),
            read_only: false,
        }];
        let plan = plan_with(&SecurityConfig::default(), &grants, &host()).unwrap();
        let b = plan
            .binds
            .iter()
            .find(|b| b.source == Path::new("/srv/other-repo"))
            .expect("an approved grant must appear in the next plan");
        assert_eq!(b.mode, BindMode::ReadWrite);
        assert!(b.why.contains("approved by the user"));
    }

    #[test]
    fn read_only_grants_stay_read_only() {
        let grants = [Grant {
            path: PathBuf::from("/srv/reference"),
            read_only: true,
        }];
        let plan = plan_with(&SecurityConfig::default(), &grants, &host()).unwrap();
        let b = plan
            .binds
            .iter()
            .find(|b| b.source == Path::new("/srv/reference"))
            .unwrap();
        assert_eq!(b.mode, BindMode::ReadOnly);
    }

    /// The credential gate, enforced where it actually matters: even a grant that
    /// somehow got approved cannot be turned into a bind.
    #[test]
    fn refuses_a_grant_for_every_preset_credential_path() {
        for src in cowboy_core::presets::all_credential_sources() {
            let abs = FakeHost::new().expand(src).unwrap();
            let grants = [Grant {
                path: abs.clone(),
                read_only: true,
            }];
            let err = plan_with(&SecurityConfig::default(), &grants, &host())
                .expect_err(&format!("{src} must be refused"));
            let msg = err.to_string();
            assert!(
                msg.contains("cowboy secrets add"),
                "refusal for {src} should point at cowboy secrets: {msg}"
            );
        }
    }

    #[test]
    fn refuses_a_grant_exposing_host_owned_config() {
        for p in [
            "/srv/proj/.cowboy",
            "/srv/proj/.cowboy/security.yaml",
            "/elsewhere/models.yaml",
        ] {
            let grants = [Grant {
                path: PathBuf::from(p),
                read_only: true,
            }];
            let err = plan_with(&SecurityConfig::default(), &grants, &host())
                .expect_err(&format!("{p} must be refused"));
            assert!(matches!(err, Error::SecurityInvariant(_)));
        }
    }

    /// A configured mount is re-checked too: the personal overlay is merged after
    /// `SecurityConfig::validate` has already run.
    #[test]
    fn refuses_a_configured_mount_that_exposes_credentials() {
        let mut sec = SecurityConfig::default();
        sec.container.mounts.push(Mount {
            source: "/home/dev/.aws".into(),
            target: "/workspace/aws".into(),
            mode: "ro".into(),
        });
        let err = plan_with(&sec, &[], &host()).expect_err("must refuse");
        assert!(matches!(err, Error::SecurityInvariant(_)));
    }

    #[test]
    fn optional_missing_credential_is_skipped_and_required_one_fails() {
        let mut sec = SecurityConfig::default();
        sec.secrets.files.push(SecretMount {
            source: "~/.config/gh".into(),
            target: "/tmp/.config/gh".into(),
            read_only: true,
            required: false,
            approval: None,
        });
        // Absent from the fake host: skipped without complaint.
        let plan = plan_with(&sec, &[], &host()).unwrap();
        assert!(!plan.binds.iter().any(|b| b.target == "/tmp/.config/gh"));

        sec.secrets.files[0].required = true;
        let err = plan_with(&sec, &[], &host()).expect_err("required grant must fail");
        assert!(matches!(err, Error::Invalid(_)));

        // Present: bound read-only.
        let probe = host().with_existing(["/home/dev/.config/gh"]);
        let plan = plan_with(&sec, &[], &probe).unwrap();
        let b = plan
            .binds
            .iter()
            .find(|b| b.target == "/tmp/.config/gh")
            .unwrap();
        assert_eq!(b.mode, BindMode::ReadOnly);
    }

    /// Landlock is derived from the binds so the two cannot disagree about what is
    /// writable.
    #[test]
    fn landlock_mirrors_the_bind_list() {
        let plan = plan_with(&SecurityConfig::default(), &[], &host()).unwrap();
        assert!(plan
            .landlock
            .read_write
            .contains(&PathBuf::from("/srv/proj")));
        assert!(plan.landlock.read_only.contains(&PathBuf::from("/usr")));
        assert!(!plan.landlock.read_write.contains(&PathBuf::from("/usr")));
    }

    /// Landlock gates ports, not addresses. That is sufficient only because the
    /// sandbox netns can reach nothing but its own loopback.
    #[test]
    fn landlock_permits_only_the_relay_port() {
        let plan = plan_with(&SecurityConfig::default(), &[], &host()).unwrap();
        assert_eq!(plan.landlock.connect_tcp, vec![8443]);
    }

    /// io_uring would otherwise be a hole straight through the seccomp filter.
    #[test]
    fn seccomp_denies_io_uring_and_raw_sockets() {
        let plan = plan_with(&SecurityConfig::default(), &[], &host()).unwrap();
        for s in ["io_uring_setup", "io_uring_enter", "io_uring_register"] {
            assert!(plan.seccomp.denied.contains(&s), "{s} must be denied");
        }
        assert!(plan.seccomp.deny_raw_sockets);
    }

    #[test]
    fn cpu_limit_bounds_build_parallelism() {
        let mut sec = SecurityConfig::default();
        sec.container.cpus = Some(config::CpuLimit::Cores(4.0));
        let plan = plan_with(&sec, &[], &host()).unwrap();
        assert_eq!(plan.limits.jobs, Some(4));
        let makeflags = plan
            .env
            .iter()
            .find(|(k, _)| k == "MAKEFLAGS")
            .map(|(_, v)| v.clone());
        assert_eq!(makeflags.as_deref(), Some("-j4"));
    }

    #[test]
    fn memory_is_parsed_and_pids_are_bounded() {
        let mut sec = SecurityConfig::default();
        sec.container.memory = Some("8g".into());
        let plan = plan_with(&sec, &[], &host()).unwrap();
        assert_eq!(plan.limits.memory_mib, Some(8192));
        assert_eq!(plan.limits.pids, Some(4096), "fork-bomb resilience");
    }

    #[test]
    fn plan_snapshot() {
        let mut sec = SecurityConfig::default();
        sec.container.cpus = Some(config::CpuLimit::Cores(2.0));
        sec.container.memory = Some("4g".into());
        let probe = host().as_linked_worktree("/srv/main/.git/worktrees/wt");
        let grants = [Grant {
            path: PathBuf::from("/srv/other-repo"),
            read_only: true,
        }];
        let plan = plan_with(&sec, &grants, &probe).unwrap();
        let denylist = Denylist::build(&probe, Path::new("/srv/proj"));
        insta::assert_snapshot!(plan.render(&denylist));
    }
}
