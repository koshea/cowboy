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
    /// Scope the domain against signalling and abstract-socket-connecting outside
    /// it (Landlock ABI 6). Hardening only: no trust boundary depends on it.
    pub scope_ipc: bool,
    // Deliberately no TCP port rules. Landlock gates bind/connect by *port*, not
    // address, so it cannot distinguish the agent's own dev server from the
    // internet: denying binds breaks `agent.yaml` processes, and allowing only the
    // relay port stops the agent reaching those processes. It would also add
    // nothing — the sandbox network namespace has no host-connected device, so all
    // egress is already forced through the transport into the policy engine. See
    // `cowboy_cli::sandbox::lockdown`.
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
    /// Not redundant with the cgroup quota, though for a different reason than you
    /// might expect. Modern coreutils `nproc` *does* read `cpu.max` (measured: a
    /// 4-core quota reports 4 while the affinity mask still shows all 32), and so do
    /// Rust's `available_parallelism` and the JVM. But plenty of tools do not —
    /// Node's `os.cpus()` reports every host core — and a build that sizes itself
    /// from 32 cores under a 4-core quota does not fail, it thrashes.
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
/// A fixed top-level path because it must not be shadowed: `/proc` and `/dev` are
/// mounted before the binds and the plan refuses any bind that would cover them,
/// and `/usr` is a read-only bind of the host's, so a mount point cannot be created
/// inside it. The leading dot keeps it out of the way of anything a project might
/// use.
pub const SHIM_PATH: &str = "/.cowboy-shim";

/// Host directories exposed read-only so the agent can use the machine's own
/// toolchain. This is the flexibility the Docker image could not offer: the agent
/// gets the compilers, language runtimes and CLIs the user actually has, at the
/// versions they actually installed, with nothing to build or pull.
const HOST_TOOLCHAIN_DIRS: &[&str] = &["/usr", "/opt"];

/// The user's own tool directories, exposed read-only when `sandbox.host_tools` is on.
///
/// `/usr` covers what the system package manager installed and nothing else, which
/// leaves the agent with a quietly *different* toolchain from the person directing it.
/// On a machine where `cargo` is a rustup shim in `~/.cargo/bin`, the agent silently
/// got Gentoo's `/usr/bin/cargo` instead — a different version — and nothing installed
/// with `pipx`, `uv tool`, `npm -g --prefix=~/.local`, `go install` or `cargo install`
/// existed at all.
///
/// Bound at their **host paths**, not somewhere tidier, because that is what the
/// contents refer to: these directories are full of interpreter shebangs and symlinks
/// written as absolute host paths, and a script relocated out from under them breaks.
///
/// Read-only throughout. The agent may run the user's tools; it may not rewrite them,
/// which would be host code execution on the user's next shell command.
const HOST_USER_BIN_DIRS: &[&str] = &["~/.local/bin", "~/bin", "~/.cargo/bin", "~/go/bin"];

/// Support directories the entries in [`HOST_USER_BIN_DIRS`] resolve *into*.
///
/// Binding the `bin` directory alone is a half-measure that fails in a confusing way:
/// `~/.cargo/bin/cargo` is a rustup shim that needs `~/.rustup` to find a toolchain,
/// and much of `~/.local/bin` is symlinks into `~/.local/share/uv`. The tool appears
/// to be installed and then fails to run.
///
/// Deliberately specific rather than `~/.local/share`, which also holds `keyrings`.
/// The denylist refuses that either way — this list is checked against it like any
/// other bind — but naming the tool directories keeps the intent legible instead of
/// relying on a refusal to trim an over-broad request.
const HOST_USER_TOOL_DIRS: &[&str] = &[
    "~/.rustup",
    "~/.local/share/uv",
    "~/.local/share/pnpm",
    "~/.local/lib",
];

/// Environment variables that point a tool at its data directory, for tools that
/// would otherwise look under `$HOME` — which the sandbox redirects into the project.
///
/// Without `RUSTUP_HOME`, binding `~/.cargo/bin` gets you a rustup shim that resolves
/// on `PATH` and then refuses to run: *"could not choose a version of cargo to run,
/// because ... no default is configured"*, because it looked for its settings under
/// the redirected `HOME` and found nothing. The bind is useless without the variable,
/// so they belong together.
///
/// Only set when the directory in question was actually bound. Each points at a
/// **read-only** bind, so the tool can run what the user installed but not modify it:
/// `cargo build` works, `rustup update` does not. That is the intended asymmetry —
/// mutating the user's toolchain from inside a sandbox is not a thing an agent should
/// be able to do on its own.
///
/// `CARGO_HOME` is deliberately absent. It is where cargo *writes* its registry cache,
/// so pointing it at the read-only `~/.cargo` would break every build; it stays under
/// the sandbox's own `HOME`.
const HOST_TOOL_ENV: &[(&str, &str)] = &[
    ("~/.rustup", "RUSTUP_HOME"),
    ("~/.local/share/pnpm", "PNPM_HOME"),
];

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
///
/// Serializable because grants persist between sessions — see
/// `cowboy_cli::sandbox::grants`, which stores them **outside** the workspace so the
/// agent cannot grant itself a path.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// Session-scoped scratch directory, bound at `/tmp`, `/run` and `/var/tmp`.
    ///
    /// These were a per-command `tmpfs`, which was a real bug rather than a design:
    /// every command gets its own mount namespace, so each one got a *fresh, empty*
    /// tmpfs and anything the previous command wrote to `/tmp` had vanished. A
    /// container's `/tmp` lived as long as the container, and agents rely on that
    /// constantly — download in one command, process it in the next.
    ///
    /// A host directory rather than a shared tmpfs because the session holder does
    /// not have a mount namespace of its own to put one in (it shares the host's, and
    /// cannot mount there). The tradeoff is that scratch is now disk-backed, so a
    /// runaway write fills the disk instead of being stopped by the memory ceiling.
    pub scratch: &'a Path,
}

/// Where the sandbox's scratch filesystems are rooted inside `scratch`, and the
/// targets they are bound at.
///
/// `/run` and `/var/tmp` get the same treatment as `/tmp` for the same reason: a
/// socket or a build's intermediate output must still be there for the next command.
pub const SCRATCH_DIRS: &[(&str, &str)] =
    &[("tmp", "/tmp"), ("run", "/run"), ("var-tmp", "/var/tmp")];

impl SandboxPlan {
    /// Build the plan, or fail if configuration or a grant would breach the
    /// boundary.
    ///
    /// Ordering matters and is deliberate: host toolchain first, then the project,
    /// then credential grants, then runtime grants, and the host-owned config mask
    /// **last** — so no later entry can re-expose what the mask hid.
    pub fn build(inputs: &PlanInputs<'_>, probe: &dyn HostProbe) -> Result<Self> {
        let sec = inputs.security;
        let workdir = sec.sandbox.workdir.clone();
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

        // 1b. The user's own tools, read-only. Collected separately because the bin
        //     directories also go on `PATH` — the sandbox starts from a cleared
        //     environment, so a directory the agent cannot find is a directory it does
        //     not have.
        let mut user_bin_dirs: Vec<String> = Vec::new();
        let mut tool_env: Vec<(String, String)> = Vec::new();
        if sec.sandbox.host_tools {
            for (raw, why) in HOST_USER_BIN_DIRS
                .iter()
                .map(|r| (r, "your tools (read-only)"))
                .chain(
                    HOST_USER_TOOL_DIRS
                        .iter()
                        .map(|r| (r, "toolchain data for your tools (read-only)")),
                )
            {
                let Some(path) = probe.expand(raw) else {
                    continue;
                };
                if !probe.exists(&path) {
                    continue;
                }
                // Checked against the denylist like any other bind. These are
                // hardcoded, but the denylist is the one place that knows what counts
                // as a secret store, and a home-relative default has no business
                // being the exception to it. Read-only exposure only — see
                // `DenyReason::blocks_read_only`.
                if let Some(reason) = denylist.check(&path) {
                    if reason.blocks_read_only() {
                        continue;
                    }
                }
                let target = path.to_string_lossy().into_owned();
                if HOST_USER_BIN_DIRS.contains(raw) {
                    user_bin_dirs.push(target.clone());
                }
                if let Some((_, var)) = HOST_TOOL_ENV.iter().find(|(d, _)| *d == *raw) {
                    tool_env.push((var.to_string(), target.clone()));
                }
                binds.push(Bind::ro(path, target, why));
            }
        }

        // 2. Session-scoped scratch, EARLY so anything later can be mounted on top of
        //    it. Putting it after the grants was a bug: binding `/tmp` shadows a grant
        //    for a path *under* `/tmp`, which is exactly the hazard the ordering rules
        //    exist to prevent, and the grant tests caught it immediately.
        for (sub, target) in SCRATCH_DIRS {
            binds.push(Bind::rw(
                inputs.scratch.join(sub),
                *target,
                "session scratch (survives between commands, not between sessions)",
            ));
        }

        // 3. The project and any other configured mounts. The default config
        //    mounts `.` at the workdir, so the project arrives through here rather
        //    than being hardcoded — one source of truth, and no second bind that
        //    could silently downgrade the project to read-only by landing later.
        let mut mounts_workdir = false;
        for m in &sec.sandbox.mounts {
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

        // 4. A linked worktree's shared git dir, at its own host path so the
        //    absolute gitdir reference in `.git` resolves. Writable so the
        //    worktree's branch can write objects and refs.
        if let Some(common) = probe.git_common_dir(inputs.root) {
            let t = common.to_string_lossy().into_owned();
            binds.push(Bind::rw(common, t, "shared git dir (linked worktree)"));
        }

        // 5. Credential grants from host-owned config. Deliberate and out-of-band:
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

        // 6. Runtime grants. Re-checked against the denylist here as well as at
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

        // 7. Mask host-owned config LAST. It lives under the project directory, so
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
        let env = build_env(sec, &workdir, &limits, &user_bin_dirs, tool_env);
        let proc_at = "/proc".to_string();
        let dev_at = "/dev".to_string();

        // The special filesystems are mounted *before* the binds, so that a bind for a
        // path under one of them lands inside it rather than being shadowed by it.
        // That means order no longer prevents a bind from shadowing them, so refuse it
        // here instead: a bind over /proc would let the agent present a fabricated
        // /proc to its own tooling, and one over /dev could hand it a device node of
        // its choosing.
        for b in &binds {
            for special in [&proc_at, &dev_at] {
                if &b.target == special || Path::new(special).starts_with(&b.target) {
                    return Err(Error::SecurityInvariant(format!(
                        "bind target {} would shadow {special}, which must be the kernel's own. \
                         Remove it from .cowboy/security.yaml.",
                        b.target
                    )));
                }
            }
        }

        let landlock = landlock_for(&binds, &proc_at, &dev_at);

        Ok(Self {
            binds,
            proc_at,
            dev_at,
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
        s.push_str(&format!("  proc {}   dev {}\n", self.proc_at, self.dev_at));

        s.push_str("\nlandlock\n");
        s.push_str(&format!(
            "  {} read-only, {} read-write paths\n",
            self.landlock.read_only.len(),
            self.landlock.read_write.len()
        ));
        s.push_str("  network: not gated here (port-only rules cannot express it)\n");
        s.push_str(&format!("  ipc scoping: {}\n", self.landlock.scope_ipc));

        s.push_str("\nseccomp\n");
        s.push_str(&format!(
            "  {} syscalls denied (incl. io_uring_setup), raw sockets denied: {}\n",
            self.seccomp.denied.len(),
            self.seccomp.deny_raw_sockets
        ));

        s.push_str("\nlimits\n");
        // Spelled out rather than debug-printed: this command exists to be read, and
        // `memory Some(8192) MiB` is not a sentence anyone wants to parse.
        let show = |v: Option<String>| v.unwrap_or_else(|| "unlimited".to_string());
        s.push_str(&format!(
            "  memory {}, cpu {}, processes {}, build jobs {}\n",
            show(self.limits.memory_mib.map(|m| format!("{m} MiB"))),
            show(self.limits.cpus.map(|c| format!("{c} cores"))),
            show(self.limits.pids.map(|p| p.to_string())),
            show(self.limits.jobs.map(|j| j.to_string())),
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

/// Landlock rules for the sandbox-internal view.
///
/// **Uses bind targets, not sources.** The shim applies these from *inside* the
/// sandbox, so paths must be as they appear there. Deriving them from the host-side
/// sources looks plausible and silently does nothing useful: `/usr` happens to have
/// the same path inside and out, so a toolchain read appears to work, while the
/// project (`/srv/x` outside, `/workspace` inside) gets no rule at all and every
/// write is denied.
///
/// The special filesystems must be included too. They are not binds, so deriving
/// rules only from the bind list leaves `/proc` and `/dev` unreadable — which breaks
/// anything that reads `/proc/self/*`.
fn landlock_for(binds: &[Bind], proc_at: &str, dev_at: &str) -> LandlockRules {
    let mut read_only = Vec::new();
    let mut read_write = Vec::new();
    for b in binds {
        match b.mode {
            BindMode::ReadOnly => read_only.push(PathBuf::from(&b.target)),
            BindMode::ReadWrite => read_write.push(PathBuf::from(&b.target)),
        }
    }
    // The virtual filesystems, writable. Each is created fresh for every command, and
    // `/proc/sys` needs privileges we do not have regardless, so finer-grained rules
    // here would cost compatibility for no gain.
    read_write.push(PathBuf::from(proc_at));
    read_write.push(PathBuf::from(dev_at));
    LandlockRules {
        read_only,
        read_write,
        scope_ipc: true,
    }
}

/// Resolve `auto` limits and the build parallelism that follows from the CPU quota.
fn resolve_limits(sec: &SecurityConfig) -> ResourceLimits {
    let cpus = sec.sandbox.cpus.as_ref().map(|c| match c {
        config::CpuLimit::Auto => config::auto_cpus(num_cpus()),
        config::CpuLimit::Cores(n) => *n,
    });
    let memory_mib = sec.sandbox.memory.as_deref().and_then(|m| {
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
/// `PATH` for the sandbox: the user's tool directories first, then the system's.
///
/// Set explicitly because the environment is cleared, and until now nothing set it —
/// the shell's compiled-in fallback happened to be reasonable, which is not the same
/// as it being decided. A bound directory the shell does not look in is a directory
/// the agent does not have.
///
/// User directories come **first**, which is where they sit in the user's own `PATH`
/// and is the point of the exercise: the agent should resolve `cargo` to the same
/// binary its user does, not to a different version of it further down.
fn sandbox_path(user_bin_dirs: &[String]) -> String {
    const SYSTEM: &[&str] = &[
        "/usr/local/sbin",
        "/usr/local/bin",
        "/usr/sbin",
        "/usr/bin",
        "/sbin",
        "/bin",
    ];
    user_bin_dirs
        .iter()
        .map(String::as_str)
        .chain(SYSTEM.iter().copied())
        .collect::<Vec<_>>()
        .join(":")
}

fn build_env(
    sec: &SecurityConfig,
    workdir: &str,
    limits: &ResourceLimits,
    user_bin_dirs: &[String],
    tool_env: Vec<(String, String)>,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("HOME".to_string(), format!("{workdir}/.cowboy/home")),
        ("COWBOY_SANDBOX".to_string(), "1".to_string()),
        ("PATH".to_string(), sandbox_path(user_bin_dirs)),
    ];
    env.extend(tool_env);
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
            scratch: Path::new("/scratch"),
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

    /// A host with the user's own tool directories present.
    fn host_with_user_tools() -> FakeHost {
        host().with_existing([
            "/home/dev/.local/bin",
            "/home/dev/.cargo/bin",
            "/home/dev/go/bin",
            "/home/dev/.rustup",
            "/home/dev/.local/share/uv",
        ])
    }

    fn ro_targets(plan: &SandboxPlan) -> Vec<&str> {
        plan.binds
            .iter()
            .filter(|b| b.mode == BindMode::ReadOnly)
            .map(|b| b.target.as_str())
            .collect()
    }

    fn env_of<'a>(plan: &'a SandboxPlan, key: &str) -> Option<&'a str> {
        plan.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The user's tools are exposed read-only and **at their host paths**. The path is
    /// not cosmetic: these directories are full of absolute interpreter shebangs and
    /// symlinks, so a script relocated somewhere tidier stops working.
    #[test]
    fn the_users_own_tool_directories_are_exposed_read_only() {
        let sec = SecurityConfig::default();
        let plan = plan_with(&sec, &[], &host_with_user_tools()).unwrap();
        for dir in [
            "/home/dev/.local/bin",
            "/home/dev/.cargo/bin",
            "/home/dev/go/bin",
            "/home/dev/.rustup",
            "/home/dev/.local/share/uv",
        ] {
            let bind = plan
                .binds
                .iter()
                .find(|b| b.target == dir)
                .unwrap_or_else(|| panic!("{dir} should be exposed"));
            assert_eq!(bind.source, Path::new(dir), "bound at its host path");
            assert_eq!(bind.mode, BindMode::ReadOnly, "{dir} must not be writable");
        }
        // A directory the host does not have is simply absent, not a failure.
        assert!(!ro_targets(&plan).contains(&"/home/dev/bin"));
    }

    /// A bound directory the shell does not search is a directory the agent does not
    /// have: the environment is cleared, so `PATH` has to be set here. The user's
    /// directories come first, so `cargo` resolves to the same binary its user gets
    /// rather than to a different version further down.
    #[test]
    fn path_puts_the_users_tools_ahead_of_the_system() {
        let sec = SecurityConfig::default();
        let plan = plan_with(&sec, &[], &host_with_user_tools()).unwrap();
        let path = env_of(&plan, "PATH").expect("PATH must be set explicitly");
        let entries: Vec<&str> = path.split(':').collect();
        let local = entries
            .iter()
            .position(|e| *e == "/home/dev/.local/bin")
            .expect("~/.local/bin on PATH");
        let usr = entries
            .iter()
            .position(|e| *e == "/usr/bin")
            .expect("/usr/bin on PATH");
        assert!(local < usr, "the user's tools come first: {path}");
        // Only bin directories go on PATH; the data directories are not searched.
        assert!(!entries.contains(&"/home/dev/.rustup"));
    }

    /// Binding `~/.cargo/bin` without `RUSTUP_HOME` yields a shim that resolves and
    /// then refuses to run, because it looks for its settings under the redirected
    /// `HOME`. `CARGO_HOME` must NOT be redirected the same way: that is where cargo
    /// *writes* its registry cache, and pointing it at a read-only bind breaks builds.
    #[test]
    fn a_bound_toolchain_dir_gets_the_variable_that_finds_it() {
        let sec = SecurityConfig::default();
        let plan = plan_with(&sec, &[], &host_with_user_tools()).unwrap();
        assert_eq!(env_of(&plan, "RUSTUP_HOME"), Some("/home/dev/.rustup"));
        assert_eq!(
            env_of(&plan, "CARGO_HOME"),
            None,
            "CARGO_HOME must stay writable under the sandbox's own HOME"
        );
        // Not set when the directory is not there to point at.
        let bare = plan_with(&sec, &[], &host()).unwrap();
        assert_eq!(env_of(&bare, "RUSTUP_HOME"), None);
    }

    /// `host_tools: false` is a real off switch, for a sandbox that should see only
    /// what the system package manager installed.
    #[test]
    fn host_tools_can_be_turned_off() {
        let sec = SecurityConfig {
            sandbox: cowboy_core::config::SandboxConfig {
                host_tools: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let plan = plan_with(&sec, &[], &host_with_user_tools()).unwrap();
        for dir in ["/home/dev/.local/bin", "/home/dev/.cargo/bin"] {
            assert!(!ro_targets(&plan).contains(&dir), "{dir} should be absent");
        }
        let path = env_of(&plan, "PATH").unwrap();
        assert!(!path.contains("/home/dev"), "no user dirs on PATH: {path}");
        // The system toolchain is still there — this switch is about the user's extras.
        assert!(ro_targets(&plan).contains(&"/usr"));
    }

    /// The default list is checked against the denylist like any other bind. A
    /// home-relative default has no business being the exception to the one place that
    /// knows what a secret store is.
    #[test]
    fn a_tool_directory_that_the_denylist_refuses_is_not_exposed() {
        // `~/.local/share/keyrings` is a denied store; the plan must never expose it,
        // and the specific tool directories it lists must not widen to cover it.
        let sec = SecurityConfig::default();
        let plan = plan_with(&sec, &[], &host_with_user_tools()).unwrap();
        for t in ro_targets(&plan) {
            assert!(
                !t.ends_with("/.local/share") && !t.contains("keyrings"),
                "{t} would expose a secret store"
            );
        }
    }

    /// `~/.cargo/bin` is where `cargo install` puts things — including cowboy itself on
    /// most machines. The denylist refuses that directory because a *writable* grant
    /// for it would be host code execution, but a read-only bind is not that hazard:
    /// the agent can already read and execute the cowboy binary, which the plan binds
    /// at `SHIM_PATH` by design. Refusing here would cost every `cargo install` user
    /// their tools to protect nothing.
    #[test]
    fn a_read_only_bind_is_not_refused_merely_for_containing_the_cowboy_binary() {
        let probe = FakeHost {
            self_exe: Some(PathBuf::from("/home/dev/.cargo/bin/cowboy")),
            ..host_with_user_tools()
        };
        let sec = SecurityConfig::default();
        let plan = plan_with(&sec, &[], &probe).unwrap();
        assert!(ro_targets(&plan).contains(&"/home/dev/.cargo/bin"));

        // …but a runtime grant for the same path is still refused, because that is the
        // writable route this protects.
        let denylist = Denylist::build(&probe, Path::new("/srv/proj"));
        let reason = denylist
            .check(Path::new("/home/dev/.cargo/bin"))
            .expect("a grant for it must still be refused");
        assert!(!reason.blocks_read_only(), "but only for write access");
    }

    /// The invariant with a dedicated E2E test under Docker, preserved here.
    #[test]
    fn masks_host_owned_config() {
        let sec = SecurityConfig::default();
        let plan = plan_with(&sec, &[], &host()).unwrap();
        let workdir = &sec.sandbox.workdir;
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
            .find(|b| b.target == sec.sandbox.workdir)
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
            .filter(|b| b.target == sec.sandbox.workdir)
            .count();
        assert_eq!(n, 1, "duplicate workdir binds: {:#?}", plan.binds);
    }

    /// A config with nothing at the workdir is a mistake worth naming, not a
    /// session with an empty project directory.
    /// The special filesystems are mounted before the binds now, so order no
    /// longer stops a bind shadowing them — this check does.
    #[test]
    fn refuses_a_bind_that_would_shadow_proc_or_dev() {
        for target in ["/proc", "/dev", "/"] {
            let mut sec = SecurityConfig::default();
            sec.sandbox.mounts.push(Mount {
                source: "/srv/proj".into(),
                target: target.into(),
                mode: "rw".into(),
            });
            let err = plan_with(&sec, &[], &host())
                .expect_err(&format!("a bind over {target} must be refused"));
            assert!(matches!(err, Error::SecurityInvariant(_)), "{err}");
        }
    }

    #[test]
    fn refuses_config_with_no_mount_at_the_workdir() {
        let mut sec = SecurityConfig::default();
        sec.sandbox.mounts.clear();
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
        sec.sandbox.mounts.push(Mount {
            source: "/home/dev/.aws".into(),
            target: "/workspace/aws".into(),
            mode: "ro".into(),
        });
        let err = plan_with(&sec, &[], &host()).expect_err("must refuse");
        assert!(matches!(err, Error::SecurityInvariant(_)));
    }

    /// A `..` in a mount source must not walk past the credential denylist.
    ///
    /// `Denylist::check` is a component-wise prefix test and says so — it requires an
    /// absolute, normalized path. Grant paths get canonicalized by both callers
    /// (`cowboy grant` and the agent's `request_path`), but mount sources go through
    /// `resolve_source`, which only joins. So `/home/dev/.aws/../.aws` did not match the
    /// denied entry `/home/dev/.aws` and was bound into the sandbox.
    ///
    /// Only reachable from host-owned `security.yaml` (or the user's personal overlay),
    /// so this is a footgun rather than an agent-exploitable hole — but the whole point
    /// of the check is to catch a user mounting their credentials by accident, and it
    /// silently failed to.
    #[test]
    fn a_mount_source_cannot_walk_past_the_denylist_with_dotdot() {
        for source in [
            "/home/dev/.aws/../.aws",
            "/home/dev/./.aws",
            "/home/dev/.config/../.aws",
            "/home/dev/.aws/",
        ] {
            let mut sec = SecurityConfig::default();
            sec.sandbox.mounts.push(Mount {
                source: source.into(),
                target: "/workspace/aws".into(),
                mode: "ro".into(),
            });
            let err = plan_with(&sec, &[], &host())
                .expect_err(&format!("mount source {source} must be refused"));
            assert!(
                matches!(err, Error::SecurityInvariant(_)),
                "{source}: {err:?}"
            );
        }
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

    /// Landlock rules are derived from the binds so the two cannot disagree about
    /// what is writable — but they must use the **sandbox-internal** targets,
    /// because the shim applies them from inside the sandbox. Host sources silently
    /// produce a domain that allows nothing useful: `/usr` has the same path either
    /// side so a toolchain read appears to work, while the project gets no rule.
    #[test]
    fn landlock_uses_sandbox_paths_not_host_paths() {
        let plan = plan_with(&SecurityConfig::default(), &[], &host()).unwrap();
        assert!(
            plan.landlock
                .read_write
                .contains(&PathBuf::from("/workspace")),
            "the project must be writable by its in-sandbox path: {:?}",
            plan.landlock.read_write
        );
        assert!(
            !plan
                .landlock
                .read_write
                .contains(&PathBuf::from("/srv/proj")),
            "the host path is meaningless inside the sandbox"
        );
        assert!(plan.landlock.read_only.contains(&PathBuf::from("/usr")));
        assert!(!plan.landlock.read_write.contains(&PathBuf::from("/usr")));
    }

    /// The virtual and scratch filesystems are not binds, so rules derived from the
    /// bind list alone leave them unreadable — which breaks anything reading
    /// `/proc/self/*`.
    #[test]
    fn landlock_includes_the_special_filesystems() {
        let plan = plan_with(&SecurityConfig::default(), &[], &host()).unwrap();
        for p in ["/proc", "/dev", "/tmp", "/run", "/var/tmp"] {
            assert!(
                plan.landlock.read_write.contains(&PathBuf::from(p)),
                "{p} must be in the Landlock domain: {:?}",
                plan.landlock.read_write
            );
        }
    }

    /// Landlock must NOT gate TCP: port-only rules cannot distinguish the agent's
    /// own dev server from the internet, so denying binds breaks `agent.yaml`
    /// processes and allowing only the relay port stops the agent reaching them.
    /// Egress is the transport's job.
    #[test]
    fn landlock_does_not_gate_the_network() {
        let plan = plan_with(&SecurityConfig::default(), &[], &host()).unwrap();
        assert!(plan.landlock.scope_ipc, "ipc scoping is still wanted");
        let rendered = plan.render(&Denylist::build(&host(), Path::new("/srv/proj")));
        assert!(rendered.contains("not gated here"), "{rendered}");
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
        sec.sandbox.cpus = Some(config::CpuLimit::Cores(4.0));
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
        sec.sandbox.memory = Some("8g".into());
        let plan = plan_with(&sec, &[], &host()).unwrap();
        assert_eq!(plan.limits.memory_mib, Some(8192));
        assert_eq!(plan.limits.pids, Some(4096), "fork-bomb resilience");
    }

    #[test]
    fn plan_snapshot() {
        let mut sec = SecurityConfig::default();
        sec.sandbox.cpus = Some(config::CpuLimit::Cores(2.0));
        sec.sandbox.memory = Some("4g".into());
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
