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

use crate::denylist::{DenyReason, Denylist};
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
    /// If `true`, the executor MUST abort when the source is missing at spawn time
    /// rather than skip it (`--ro-bind` not `--ro-bind-try`). Used for the config
    /// **mask**: it is the one bind whose *absence widens* the boundary (a skipped
    /// mask leaves `security.yaml` exposed), so a missing source must fail closed.
    /// Every other bind is optional — a source that vanished between planning and
    /// spawn should not abort the command.
    pub required: bool,
}

impl Bind {
    fn ro(source: impl Into<PathBuf>, target: impl Into<String>, why: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            mode: BindMode::ReadOnly,
            why: why.into(),
            required: false,
        }
    }
    /// A read-only bind whose source must exist at spawn (fail-closed): the executor
    /// aborts rather than silently skipping it. For the config mask.
    fn ro_required(
        source: impl Into<PathBuf>,
        target: impl Into<String>,
        why: impl Into<String>,
    ) -> Self {
        Self {
            required: true,
            ..Self::ro(source, target, why)
        }
    }
    fn rw(source: impl Into<PathBuf>, target: impl Into<String>, why: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            mode: BindMode::ReadWrite,
            why: why.into(),
            required: false,
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
    /// Directories that may be opened and listed, and nothing more (`ReadDir`,
    /// no file reads). Just the sandbox root: without it `/` and the skeleton
    /// directories bwrap creates to hold bind targets (`/etc`, `/home`, …) can't be
    /// opened at all, which breaks code that resolves paths from a root dirfd
    /// (`open("/", O_RDONLY)` then `openat`). Widens nothing: the root is a fresh
    /// tmpfs holding only what the plan binds, hidden paths are never bound or are
    /// masked by an overmount, and every bind already carries `ReadDir`.
    pub list_dirs: Vec<PathBuf>,
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

/// A host directory exposed **copy-on-write**: readable at `target`, with every
/// write landing in `upper` instead of the host's copy.
///
/// The motivating case is a language-version store (mise). A plain read-only bind
/// is not enough — the tool must be able to install a version the host lacks, and a
/// read-only store fails the install outright — while a read-write bind would let
/// the agent rewrite a binary the user runs on the host afterwards, which is the
/// one thing [`Bind::ro`] on the toolchain directories exists to prevent.
///
/// An overlay keeps both: the host's store is the immutable lower layer, so
/// everything already installed is reused with nothing to download, and anything
/// the sandbox installs or modifies is diverted into `upper`, which lives with the
/// project. The host's copy is never written to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay {
    /// The host directory read through the overlay. Never written to.
    pub lower: PathBuf,
    /// Where writes land. Persistent, so an install survives the session.
    pub upper: PathBuf,
    /// overlayfs scratch. Must be an empty directory on the same filesystem as
    /// `upper`, and is managed by the kernel, not by us.
    pub work: PathBuf,
    /// Where the merged view appears inside the sandbox.
    pub target: String,
    /// Why this overlay exists, for `cowboy sandbox plan`.
    pub why: String,
}

/// Which confinement mechanism a plan is built for.
///
/// A plan input rather than a `cfg`, so the macOS plan is built and snapshot-tested
/// on a Linux CI runner and the other way round: what the boundary *is* stays
/// reviewable wherever the tests run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// Namespaces + bwrap + Landlock + seccomp. Paths are remapped (the project
    /// appears at the workdir).
    Linux,
    /// Seatbelt. Nothing can be remapped, so everything appears at its host path;
    /// exposures become allow rules and the config mask becomes a deny rule.
    MacOs,
}

impl Platform {
    /// The platform this binary runs on.
    pub const fn host() -> Self {
        if cfg!(target_os = "macos") {
            Platform::MacOs
        } else {
            Platform::Linux
        }
    }
}

/// Everything needed to confine and run one command.
#[derive(Debug, Clone)]
pub struct SandboxPlan {
    pub platform: Platform,
    pub binds: Vec<Bind>,
    /// Copy-on-write exposures, applied after [`Self::binds`] so an overlay can be
    /// mounted onto a path a bind created.
    pub overlays: Vec<Overlay>,
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
    /// Paths the command may neither read nor write, whatever else allows (macOS: the
    /// host-owned config, which Linux masks with a bind instead). Rendered **last**.
    pub masks: Vec<PathBuf>,
    /// Symlinks to create in the agent's `HOME` before the command runs, as
    /// `(link, target)` (macOS). Stands in for a bind whose Linux target is under
    /// [`AGENT_HOME`] — `~/.aws/credentials` from a credential grant — since Seatbelt
    /// can expose a path but not move it.
    pub home_links: Vec<(PathBuf, PathBuf)>,
    /// Where the xcrun developer shims keep their lookup cache, whatever `TMPDIR`
    /// says (macOS). Readable, never writable: the host's own `xcrun` trusts it.
    pub xcrun_cache_prefix: Option<PathBuf>,
    /// Things in the configuration this platform cannot honour, for `cowboy sandbox
    /// plan` to say out loud rather than silently drop.
    pub notes: Vec<String>,
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
/// would otherwise look under `$HOME` — which the sandbox redirects to
/// [`AGENT_HOME`].
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

/// The host's mise store, shared copy-on-write when `share_mise_store` is on.
///
/// Not in [`HOST_USER_TOOL_DIRS`] because a read-only bind is the wrong shape for
/// it: Cowboy runs `mise install` itself at session start, and against a read-only
/// store that fails with `Permission denied` the moment a project pins a version
/// the host does not have. It gets an [`Overlay`] instead — see that type.
///
/// Exposed at its **host path**, like every other tool directory, because installs
/// are full of absolute shebangs and symlinks into their own prefix.
const HOST_MISE_STORE: &str = "~/.local/share/mise";

/// Files whose presence in the project root means the project uses mise, so the
/// copy-on-write mise store is worth mounting. Matches `native.rs::has_mise_config`;
/// the plan and the executor must agree on what "uses mise" means.
///
/// RECONSTRUCTED after these lines were lost to an accidental `git checkout` of this
/// file. The five entries below were recovered verbatim; if the original list also
/// carried `.tool-versions` (mise reads it for asdf compatibility) that entry needs
/// adding back, and a project configured only that way currently gets no overlay.
pub const PROJECT_MISE_CONFIGS: &[&str] = &[
    "mise.toml",
    ".mise.toml",
    "mise/config.toml",
    ".mise/config.toml",
    ".config/mise/config.toml",
];

/// Where the overlay's write layers live, relative to the project root.
///
/// Inside the project rather than the session scratch directory: scratch is
/// session-scoped, and a store that emptied itself every session would reinstall
/// every toolchain every time — the exact cost this is here to remove.
///
/// Both sit under one parent so it can carry a `.gitignore` of its own (`*`),
/// which keeps a multi-gigabyte store out of `git status` in every project without
/// depending on the project's `.gitignore` having been updated. The marker goes on
/// the parent, never in `upper` itself, because anything in `upper` shows up inside
/// the merged view — i.e. as a stray file in the user's toolchain store.
pub const MISE_OVERLAY_DIR: &str = ".cowboy/mise";
const MISE_UPPER_DIR: &str = ".cowboy/mise/upper";
const MISE_WORK_DIR: &str = ".cowboy/mise/work";

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

/// macOS directories the toolchain needs, read-only: the system, the command-line
/// tools, and Homebrew. Verified by running `cc`, `git`, `python3`, `node` and
/// `cargo` under a profile that allowed only these — see the macOS section of
/// `docs/src/security/sandbox-decisions.md`.
///
/// `/Library/Apple` is not a typo for `/System`: Xcode loads `MobileDevice.framework`
/// from there, and dyld's error names the `/System` path it tried first.
const MACOS_TOOLCHAIN_DIRS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/System",
    "/Library/Apple",
    "/Library/Developer",
    "/opt/homebrew",
    "/private/var/db/dyld",
    "/private/var/db/timezone",
    "/private/var/select",
];

/// macOS configuration files the toolchain reads, as an allowlist for the same reason
/// as [`ETC_ALLOW`]. The Xcode preference is what the `/usr/bin` developer shims
/// consult for licence acceptance: without it every one of them refuses to run.
const MACOS_FILES_ALLOW: &[&str] = &[
    "/private/etc/hosts",
    "/private/etc/ssl",
    "/private/etc/passwd",
    "/private/etc/group",
    "/private/etc/services",
    "/private/etc/protocols",
    "/private/etc/shells",
    "/private/etc/profile",
    "/private/etc/bashrc",
    "/private/etc/zshenv",
    "/private/etc/zprofile",
    "/private/etc/zshrc",
    "/private/etc/paths",
    "/private/etc/paths.d",
    "/private/etc/localtime",
    "/Library/Preferences/com.apple.dt.Xcode.plist",
    "/Library/Preferences/.GlobalPreferences.plist",
    "/Library/Preferences/Logging/com.apple.diagnosticd.filter.plist",
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
    /// Host directory bound at [`AGENT_HOME`] as the agent's `HOME`. Per-project and
    /// persistent (under the user's cache dir), so caches survive between sessions —
    /// unlike `scratch`, which is reaped with the session.
    pub agent_home: &'a Path,
    /// A host-written git config carrying the user's identity (`user.name`,
    /// `user.email` from their global config), bound read-only at
    /// [`GIT_IDENTITY_AT`] and used as git's *system* config. `None` when the host
    /// has no identity to lend.
    pub git_identity: Option<&'a Path>,
    /// Which mechanism the plan is for; see [`Platform`].
    pub platform: Platform,
}

/// Where [`PlanInputs::git_identity`] is bound, and what `GIT_CONFIG_SYSTEM` names.
///
/// System scope is the lowest precedence git has, which is the point: the agent's
/// `HOME` is its own directory, so the user's `~/.gitconfig` is never visible and
/// commits fell back to the account name for uid 0 ("Super User") — while a repo's
/// own `user.email` still applied, giving a commit half-attributed to you. As a
/// system default the user's identity fills that gap, and a repo's own
/// `user.name`/`user.email` or the agent's `git config --global` still win. The file
/// includes the host's `/etc/gitconfig` so replacing the system file loses nothing.
pub const GIT_IDENTITY_AT: &str = "/etc/cowboy/gitconfig";

/// Where the sandbox's scratch filesystems are rooted inside `scratch`, and the
/// targets they are bound at.
///
/// `/run` and `/var/tmp` get the same treatment as `/tmp` for the same reason: a
/// socket or a build's intermediate output must still be there for the next command.
pub const SCRATCH_DIRS: &[(&str, &str)] =
    &[("tmp", "/tmp"), ("run", "/run"), ("var-tmp", "/var/tmp")];

/// The sandboxed agent's `HOME`, deliberately **outside the workdir**.
///
/// It used to be `{workdir}/.cowboy/home`, i.e. inside the project. That put a
/// directory nothing should ever commit inside the repo: tool caches, and — observed
/// on a real project — a dev-secrets cache its own tooling wrote under `~`. It also
/// showed up as untracked in `git status`, was erased by `git clean -fdx`, and gave
/// every worktree of a repo its own cold copy.
///
/// The host side is per-project and lives under the user's cache directory, so it
/// persists between sessions (a warm cache is the whole point) while staying out of
/// the workspace the agent can write and the user can commit.
pub const AGENT_HOME: &str = "/home/agent";

/// Where the container put the agent's `HOME`, and so where the credential presets
/// still point their targets. Read as home-relative on macOS, which cannot place a
/// file at an arbitrary target and links it into the agent's `HOME` instead.
const LEGACY_HOME: &str = "/tmp";

impl SandboxPlan {
    /// Build the plan, or fail if configuration or a grant would breach the
    /// boundary.
    ///
    /// Ordering matters and is deliberate: host toolchain first, then the project,
    /// then credential grants, then runtime grants, and the host-owned config mask
    /// **last** — so no later entry can re-expose what the mask hid.
    pub fn build(inputs: &PlanInputs<'_>, probe: &dyn HostProbe) -> Result<Self> {
        let sec = inputs.security;
        let mac = inputs.platform == Platform::MacOs;
        // Seatbelt cannot remap, so on macOS the project is where it is on the host.
        let workdir = if mac {
            inputs.root.to_string_lossy().into_owned()
        } else {
            sec.sandbox.workdir.clone()
        };
        let configured_workdir = sec.sandbox.workdir.clone();
        // Where a host path appears inside the sandbox: on Linux, wherever the plan
        // says; on macOS, necessarily at itself.
        let at = |host: &Path, linux_target: &str| -> String {
            if mac {
                host.to_string_lossy().into_owned()
            } else {
                linux_target.to_string()
            }
        };
        let denylist = Denylist::build_for(probe, inputs.root, inputs.platform);
        let mut binds = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let mut home_links: Vec<(PathBuf, PathBuf)> = Vec::new();

        // 0. The lockdown shim: the cowboy binary itself, read-only. bwrap cannot
        //    apply Landlock, so it execs this instead of the command directly, and
        //    it must therefore be reachable inside the sandbox. Read-only, and the
        //    denylist separately prevents any runtime grant making it writable. (On
        //    macOS the shim runs before the profile applies; the file tools still run
        //    the binary from inside, so it is readable there too.)
        if let Some(exe) = probe.self_exe() {
            let target = at(&exe, SHIM_PATH);
            binds.push(Bind::ro(exe, target, "lockdown shim (cowboy binary)"));
        }

        // 1. The host's own toolchain, read-only.
        let (toolchain_dirs, config_files) = if mac {
            (MACOS_TOOLCHAIN_DIRS, MACOS_FILES_ALLOW)
        } else {
            (HOST_TOOLCHAIN_DIRS, ETC_ALLOW)
        };
        for dir in toolchain_dirs {
            let p = PathBuf::from(dir);
            if probe.exists(&p) {
                binds.push(Bind::ro(p, *dir, "host toolchain"));
            }
        }
        if mac {
            if let Some(bundle) = probe.developer_bundle().filter(|b| probe.exists(b)) {
                let target = bundle.to_string_lossy().into_owned();
                binds.push(Bind::ro(
                    bundle,
                    target,
                    "the selected Xcode (xcode-select)",
                ));
            }
        }
        for entry in config_files {
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
        let mut overlays: Vec<Overlay> = Vec::new();
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

            // The mise store, copy-on-write rather than read-only: see `Overlay`.
            // Only when this project actually uses mise — otherwise there is nothing
            // to install and no reason to create a `.cowboy/mise/` overlay in it.
            // macOS has no overlayfs, so mise keeps its own store in the agent's HOME.
            let wants_mise = sec.sandbox.share_mise_store && project_uses_mise(probe, inputs.root);
            if wants_mise && mac {
                notes.push(
                    "share_mise_store: macOS has no copy-on-write mount, so mise installs into \
                     the agent's own store instead of reusing yours"
                        .to_string(),
                );
            }
            if wants_mise && !mac {
                if let Some(lower) = probe.expand(HOST_MISE_STORE) {
                    let denied = denylist.check(&lower).is_some_and(|r| r.blocks_read_only());
                    if probe.exists(&lower) && !denied {
                        let target = lower.to_string_lossy().into_owned();
                        // Exposed at its host path, so `MISE_DATA_DIR` and the
                        // absolute paths baked into installs agree.
                        tool_env.push(("MISE_DATA_DIR".to_string(), target.clone()));
                        overlays.push(Overlay {
                            lower,
                            upper: inputs.root.join(MISE_UPPER_DIR),
                            work: inputs.root.join(MISE_WORK_DIR),
                            target,
                            why:
                                "your mise toolchains (copy-on-write; installs stay in the project)"
                                    .to_string(),
                        });
                    }
                }
            }
        }

        // 2. Session-scoped scratch, EARLY so anything later can be mounted on top of
        //    it. Putting it after the grants was a bug: binding `/tmp` shadows a grant
        //    for a path *under* `/tmp`, which is exactly the hazard the ordering rules
        //    exist to prevent, and the grant tests caught it immediately.
        //    On macOS there is only `TMPDIR`: the host's `/tmp` cannot be swapped for a
        //    private one, and it is shared with everything else the user runs.
        for (sub, target) in SCRATCH_DIRS {
            if mac && *sub != "tmp" {
                continue;
            }
            let source = inputs.scratch.join(sub);
            let target = at(&source, target);
            binds.push(Bind::rw(
                source,
                target,
                "session scratch (survives between commands, not between sessions)",
            ));
        }

        //    The agent's HOME, alongside scratch because it is the same class of thing
        //    — a writable directory the agent owns — and differs only in living outside
        //    the workspace and outliving the session.
        let agent_home = at(inputs.agent_home, AGENT_HOME);
        binds.push(Bind::rw(
            inputs.agent_home.to_path_buf(),
            agent_home.clone(),
            "the agent's HOME (per-project, persists between sessions)",
        ));

        // 3. The project and any other configured mounts. The default config
        //    mounts `.` at the workdir, so the project arrives through here rather
        //    than being hardcoded — one source of truth, and no second bind that
        //    could silently downgrade the project to read-only by landing later.
        let mut mounts_workdir = false;
        for m in &sec.sandbox.mounts {
            let source = resolve_source(inputs.root, &m.source);
            // The same invariant `SecurityConfig::validate` enforces, re-checked
            // here because a mount can also arrive via the user's personal
            // overlay, which is merged after that validation runs. Resolves symlinks
            // first, so a benign-looking source that links into a credential store is
            // caught rather than bound.
            if let Some(reason) = denied_source(&denylist, probe, &source) {
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
            if m.target == configured_workdir {
                mounts_workdir = true;
            }
            let why = if source == inputs.root {
                "the project"
            } else {
                "configured mount"
            };
            let target = if mac {
                if m.target != configured_workdir && Path::new(&m.target) != source {
                    notes.push(format!(
                        "mount target {} is ignored: macOS cannot remap paths, so {} \
                         appears at its own path",
                        m.target,
                        source.display()
                    ));
                }
                source.to_string_lossy().into_owned()
            } else {
                m.target.clone()
            };
            binds.push(Bind {
                source,
                target,
                mode,
                why: why.into(),
                required: false,
            });
        }
        // An agent with no project is never what anyone meant; say so rather than
        // starting a session whose workdir does not exist.
        if !mounts_workdir {
            return Err(Error::Invalid(format!(
                "no mount targets the workdir {configured_workdir}; the agent would have no \
                 project. Add a mount with target: {configured_workdir} to \
                 .cowboy/security.yaml."
            )));
        }
        if mac && configured_workdir != workdir {
            notes.push(format!(
                "workdir {configured_workdir} is ignored: on macOS the project is at {workdir}"
            ));
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
            let target = if mac {
                // Exposed where it is, and linked into the agent's HOME when that is
                // where the tool will look for it (`~/.aws/credentials`). The presets
                // still write `/tmp/…`, from when the container's HOME was `/tmp`;
                // either way the tool is looking in its home directory.
                let home_relative = Path::new(&grant.target)
                    .strip_prefix(AGENT_HOME)
                    .or_else(|_| Path::new(&grant.target).strip_prefix(LEGACY_HOME));
                match home_relative {
                    Ok(rel) if !rel.as_os_str().is_empty() => {
                        home_links.push((inputs.agent_home.join(rel), source.clone()));
                    }
                    _ => notes.push(format!(
                        "credential target {} is ignored: macOS cannot remap paths, so {} \
                         appears at its own path",
                        grant.target,
                        source.display()
                    )),
                }
                source.to_string_lossy().into_owned()
            } else {
                grant.target.clone()
            };
            binds.push(Bind {
                source,
                target,
                mode: if grant.read_only {
                    BindMode::ReadOnly
                } else {
                    BindMode::ReadWrite
                },
                why: "credential grant (security.yaml)".into(),
                required: false,
            });
        }

        // 6. Runtime grants. Re-checked against the denylist here as well as at
        //    approval time: this is the load-bearing check, since it is the one a
        //    persisted or hand-edited grant must also pass. Uses the same
        //    canonicalizing `denied_source` as configured mounts, so a grant for a
        //    path under an agent-writable dir that is later swapped for a symlink to
        //    a credential store (`~/.aws`, `~/.config/cowboy`) is resolved and caught
        //    rather than followed by `bwrap --bind`.
        for g in inputs.grants {
            if let Some(reason) = denied_source(&denylist, probe, &g.path) {
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
                required: false,
            });
        }

        // 7. The Ranch store is the committed *source of truth* for multi-workstream
        //    plans, and its scope (which workstreams exist, their deps, acceptance)
        //    is user-gated: only the coordinator and `ranch approve` — both host-side
        //    — ever write it. It lives inside the read-write workspace, though, so a
        //    sandboxed command could otherwise rewrite `ranch.yaml` out-of-band and
        //    have the next auto-advance ratify the tampered scope (the in-run
        //    fingerprint gate can't see an edit made between runs). Bind it read-only
        //    over itself so the agent can still *read* its own brief but the kernel
        //    refuses writes. Read-only rather than masked because the workstream agent
        //    legitimately reads the plan; the host writers are unaffected (they run
        //    outside the sandbox). Placed before the config mask so the mask stays the
        //    last bind (see `mask_binds_come_last`).
        let ranches = inputs.root.join(config::COWBOY_DIR).join("ranches");
        if probe.exists(&ranches) {
            let target = at(
                &ranches,
                &format!("{workdir}/{}/ranches", config::COWBOY_DIR),
            );
            binds.push(Bind::ro(
                ranches,
                target,
                "ranch store (user-gated, read-only)",
            ));
        }

        // 7b. The user's git identity, as git's system config (see GIT_IDENTITY_AT).
        if let Some(identity) = inputs.git_identity {
            let target = at(identity, GIT_IDENTITY_AT);
            binds.push(Bind::ro(
                identity.to_path_buf(),
                target.clone(),
                "your git identity (user.name/user.email only)",
            ));
            tool_env.push(("GIT_CONFIG_SYSTEM".to_string(), target));
        }

        // 8. Mask host-owned config LAST. It lives under the project directory, so
        //    it is inside a bind the agent can otherwise read; an empty read-only
        //    file over it means the agent cannot learn its own boundary. On macOS
        //    the same thing is a deny rule, rendered after every allow.
        let mut masks = Vec::new();
        for file in [config::SECURITY_FILE, config::MODELS_FILE] {
            let host_path = inputs.root.join(config::COWBOY_DIR).join(file);
            if mac {
                // Unconditionally: a deny on a path that does not exist yet also stops
                // the agent creating one for the host to read later.
                masks.push(host_path);
            } else if probe.exists(&host_path) {
                binds.push(Bind::ro_required(
                    inputs.mask_file.to_path_buf(),
                    format!("{workdir}/{}/{file}", config::COWBOY_DIR),
                    "mask host-owned config",
                ));
            }
        }

        // The xcrun cache: see `PlanInputs`-independent `xcrun_cache_prefix`.
        let xcrun_cache_prefix = if mac {
            probe
                .darwin_user_temp()
                .map(|t| probe.canonicalize(&t).unwrap_or(t).join("xcrun_db"))
        } else {
            None
        };

        let limits = resolve_limits(sec);
        let mut env = build_env(sec, &workdir, &limits, &user_bin_dirs, tool_env, mac);
        if mac {
            // `HOME` is the agent's own directory at its host path, and `TMPDIR` the
            // session scratch — the only temporary directory it can write.
            set_env(&mut env, "HOME", &agent_home);
            let tmp = at(&inputs.scratch.join("tmp"), "/tmp");
            set_env(&mut env, "TMPDIR", &tmp);
            // Apple's git asks the proxy for credentials only after a 407 unless
            // told otherwise; see the macOS notes in sandbox-decisions.md.
            set_env(&mut env, "GIT_HTTP_PROXY_AUTHMETHOD", "basic");
        }
        let (proc_at, dev_at) = if mac {
            (String::new(), String::new())
        } else {
            ("/proc".to_string(), "/dev".to_string())
        };

        // The special filesystems are mounted *before* the binds, so that a bind for a
        // path under one of them lands inside it rather than being shadowed by it.
        // That means order no longer prevents a bind from shadowing them, so refuse it
        // here instead: a bind over /proc would let the agent present a fabricated
        // /proc to its own tooling, and one over /dev could hand it a device node of
        // its choosing.
        for b in &binds {
            for special in [&proc_at, &dev_at] {
                if special.is_empty() {
                    continue;
                }
                if &b.target == special || Path::new(special).starts_with(&b.target) {
                    return Err(Error::SecurityInvariant(format!(
                        "bind target {} would shadow {special}, which must be the kernel's own. \
                         Remove it from .cowboy/security.yaml.",
                        b.target
                    )));
                }
            }
        }

        let (landlock, seccomp, symlinks) = if mac {
            (
                LandlockRules::default(),
                SeccompProfile {
                    denied: Vec::new(),
                    deny_raw_sockets: false,
                },
                Vec::new(),
            )
        } else {
            (
                landlock_for(&binds, &overlays, &proc_at, &dev_at),
                SeccompProfile::default(),
                USR_SYMLINKS
                    .iter()
                    .map(|(t, l)| (t.to_string(), l.to_string()))
                    .collect(),
            )
        };

        Ok(Self {
            platform: inputs.platform,
            binds,
            overlays,
            proc_at,
            dev_at,
            symlinks,
            env,
            workdir,
            landlock,
            seccomp,
            limits,
            masks,
            home_links,
            xcrun_cache_prefix,
            notes,
        })
    }

    /// A human-readable rendering, for `cowboy sandbox plan`.
    ///
    /// The boundary should be inspectable without reading the source or trusting
    /// a summary — this is what the user checks when they want to know what the
    /// agent can actually reach.
    pub fn render(&self, denylist: &Denylist) -> String {
        if self.platform == Platform::MacOs {
            return self.render_macos(denylist);
        }
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
        for o in &self.overlays {
            // Rendered as `cow` rather than ro/rw because it is neither: the source
            // is readable and never written, the writes go somewhere else, and
            // conflating it with either would misdescribe the boundary.
            s.push_str(&format!(
                "  cow {} -> {}   ({})\n       writes land in {}\n",
                o.lower.display(),
                o.target,
                o.why,
                o.upper.display()
            ));
        }
        s.push_str(&format!("  proc {}   dev {}\n", self.proc_at, self.dev_at));

        s.push_str("\nlandlock\n");
        s.push_str(&format!(
            "  {} read-only, {} read-write paths\n",
            self.landlock.read_only.len(),
            self.landlock.read_write.len()
        ));
        for d in &self.landlock.list_dirs {
            s.push_str(&format!("  list-only: {}\n", d.display()));
        }
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

    /// [`Self::render`] for a Seatbelt plan: every path is where it is on the host,
    /// so there is nothing to show but the access, and the network is a proxy rather
    /// than an interception.
    fn render_macos(&self, denylist: &Denylist) -> String {
        let mut s = String::new();
        s.push_str("filesystem (Seatbelt; everything else is denied, even to stat)\n");
        for b in &self.binds {
            let mode = match b.mode {
                BindMode::ReadOnly => "ro",
                BindMode::ReadWrite => "rw",
            };
            s.push_str(&format!("  {mode}  {}   ({})\n", b.target, b.why));
        }
        for m in &self.masks {
            s.push_str(&format!(
                "  --  {}   (host-owned config, masked)\n",
                m.display()
            ));
        }
        for (link, target) in &self.home_links {
            s.push_str(&format!(
                "  ln  {} -> {}   (credential grant, in the agent's HOME)\n",
                link.display(),
                target.display()
            ));
        }
        if let Some(p) = &self.xcrun_cache_prefix {
            s.push_str(&format!(
                "  ro  {}*   (xcrun's lookup cache; the host trusts it, so never writable)\n",
                p.display()
            ));
        }

        s.push_str("\nnetwork\n");
        s.push_str("  outbound: only this session's host proxy, which asks the policy engine\n");
        s.push_str("  loopback: bind and accept allowed; connections go through the proxy\n");
        s.push_str("  dns: denied inside the sandbox; the proxy resolves on the host\n");

        s.push_str("\nprocesses\n");
        s.push_str(
            "  no launchd jobs, AppleEvents, LaunchServices, or signals outside the sandbox\n",
        );

        s.push_str("\nlimits\n");
        s.push_str("  not enforced on macOS (no per-session cgroup equivalent)");
        match self.limits.jobs {
            Some(j) => s.push_str(&format!("; build jobs {j}\n")),
            None => s.push('\n'),
        }

        if !self.notes.is_empty() {
            s.push_str("\nnot applied on macOS\n");
            for n in &self.notes {
                s.push_str(&format!("  {n}\n"));
            }
        }

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

/// Whether this project is configured for mise, by the same list the executor uses.
///
/// Goes through the [`HostProbe`] rather than touching the filesystem directly, so a
/// plan stays buildable and assertable without a real project on disk.
///
/// RECONSTRUCTED alongside [`PROJECT_MISE_CONFIGS`] — see the note there.
fn project_uses_mise(probe: &dyn HostProbe, root: &Path) -> bool {
    PROJECT_MISE_CONFIGS
        .iter()
        .any(|f| probe.exists(&root.join(f)))
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
///
/// **Overlays need rules for the same reason**, and they are not binds either. When
/// they were missing, `share_mise_store` was silently inert: bwrap mounted the
/// overlay read-write and the upperdir was writable, but the target sat outside the
/// ruleset, so readdir, read *and* write were all denied. It surfaced as `mise
/// install` dying with an opaque `Permission denied (os error 13)` at session start,
/// while `stat` kept working throughout — Landlock has no stat access right — which
/// made it read as a broken mount rather than a missing rule.
///
/// An overlay's target is read-**write**: that is what copy-on-write means, and it
/// widens nothing. Writes copy up into the overlay's upperdir, which lives inside the
/// project the agent can already write, and overlayfs never modifies the lower — so
/// the host's own store stays untouched whatever rule is granted here.
fn landlock_for(
    binds: &[Bind],
    overlays: &[Overlay],
    proc_at: &str,
    dev_at: &str,
) -> LandlockRules {
    let mut read_only = Vec::new();
    let mut read_write = Vec::new();
    for b in binds {
        match b.mode {
            BindMode::ReadOnly => read_only.push(PathBuf::from(&b.target)),
            BindMode::ReadWrite => read_write.push(PathBuf::from(&b.target)),
        }
    }
    for o in overlays {
        read_write.push(PathBuf::from(&o.target));
    }
    // The virtual filesystems, writable. Each is created fresh for every command, and
    // `/proc/sys` needs privileges we do not have regardless, so finer-grained rules
    // here would cost compatibility for no gain.
    read_write.push(PathBuf::from(proc_at));
    read_write.push(PathBuf::from(dev_at));
    LandlockRules {
        read_only,
        read_write,
        list_dirs: vec![PathBuf::from("/")],
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
fn sandbox_path(user_bin_dirs: &[String], mac: bool) -> String {
    const SYSTEM: &[&str] = &[
        "/usr/local/sbin",
        "/usr/local/bin",
        "/usr/sbin",
        "/usr/bin",
        "/sbin",
        "/bin",
    ];
    // Homebrew first, as its own `shellenv` puts it: on Apple Silicon it is where
    // the user's `python3`, `node` and `git` actually resolve.
    const MACOS: &[&str] = &[
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ];
    user_bin_dirs
        .iter()
        .map(String::as_str)
        .chain(if mac { MACOS } else { SYSTEM }.iter().copied())
        .collect::<Vec<_>>()
        .join(":")
}

/// Replace (or add) one variable in a built environment, keeping it sorted.
fn set_env(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    env.retain(|(k, _)| k != key);
    env.push((key.to_string(), value.to_string()));
    env.sort();
}

fn build_env(
    sec: &SecurityConfig,
    workdir: &str,
    limits: &ResourceLimits,
    user_bin_dirs: &[String],
    tool_env: Vec<(String, String)>,
    mac: bool,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("HOME".to_string(), AGENT_HOME.to_string()),
        ("COWBOY_SANDBOX".to_string(), "1".to_string()),
        ("PATH".to_string(), sandbox_path(user_bin_dirs, mac)),
        // mise refuses to parse a config carrying `[env]` or `[tasks]` until it is
        // trusted, and trust is recorded under $HOME — which is a fresh directory
        // inside every sandbox, so a `mise trust` on the host never carries in and
        // the project is untrusted on every run. That made `mise install` (which
        // Cowboy runs itself on any project with a mise config) fail outright.
        //
        // Scoped to the workdir rather than a blanket trust-everything: the prompt
        // exists to stop a repo running code merely because you cd into it, and
        // inside the sandbox Cowboy is already deliberately executing this
        // project's toolchain. It is not part of the boundary — the kernel is —
        // so nothing is weakened by trusting the one directory the user aimed
        // Cowboy at, while configs from anywhere else still require a decision.
        ("MISE_TRUSTED_CONFIG_PATHS".to_string(), workdir.to_string()),
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

/// Run the credential denylist against a bind source, resolving symlinks first.
///
/// The denylist's own matching is purely lexical, so a symlink whose name looks
/// innocent but points at `~/.aws` or `~/.config/cowboy` would pass. Canonicalizing
/// through the probe makes the check see the real destination. Both the literal and
/// (when it differs) the resolved path are checked: the literal catches a denied path
/// that does not yet exist (so cannot be canonicalized), the resolved one catches the
/// symlink redirection. Returns the refusal reason if either is denied.
fn denied_source(denylist: &Denylist, probe: &dyn HostProbe, source: &Path) -> Option<DenyReason> {
    if let Some(reason) = denylist.check(source) {
        return Some(reason);
    }
    if let Some(real) = probe.canonicalize(source) {
        if real != source {
            if let Some(reason) = denylist.check(&real) {
                return Some(reason);
            }
        }
    }
    None
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

/// Total host memory in MiB, 0 when unreadable (which makes `auto` clamp to its floor
/// rather than guess high).
#[cfg(target_os = "macos")]
fn host_mem_mib() -> u64 {
    let mut bytes: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: `hw.memsize` is a u64 sysctl; the buffer and its length describe one.
    let rc = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut bytes).cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 {
        bytes / (1024 * 1024)
    } else {
        0
    }
}

/// Total host memory in MiB from `/proc/meminfo`, 0 when unreadable (which makes
/// `auto` clamp to its floor rather than guess high).
#[cfg(not(target_os = "macos"))]
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
            agent_home: Path::new("/cache/cowboy/home/proj"),
            git_identity: None,
            platform: Platform::Linux,
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

    /// mise records trust under $HOME, and the sandbox's $HOME is fresh every run —
    /// so a project config with `[env]` or `[tasks]` was untrusted on every start and
    /// the `mise install` Cowboy runs itself failed with "Config files … are not
    /// trusted". Trust is scoped to the workdir, never granted globally.
    #[test]
    fn the_projects_own_mise_config_is_trusted_inside_the_sandbox() {
        let sec = SecurityConfig::default();
        let plan = plan_with(&sec, &[], &host()).unwrap();
        let trusted = env_of(&plan, "MISE_TRUSTED_CONFIG_PATHS")
            .expect("mise config in the workdir must be trusted or `mise install` fails");
        // It is the workdir itself, so nested configs are covered too. Asserted against
        // the workdir directly: this used to check that `HOME` started with the trust
        // path, which only worked while `HOME` lived *inside* the workspace and said
        // nothing about the project's config once it moved out.
        assert_eq!(
            trusted, plan.workdir,
            "the workdir itself must be the trust path"
        );
        // …and not the filesystem root, which would trust every config anywhere.
        assert_ne!(trusted, "/", "must not trust configs outside the project");
        assert!(!trusted.is_empty());
    }

    /// The agent's `HOME` must live outside the workdir.
    ///
    /// It was `{workdir}/.cowboy/home`, which wrote tool caches — and on a real project
    /// a dev-secrets cache the project's own tooling put under `~` — into the repo,
    /// where they showed up as untracked files and could be committed. Nothing the agent
    /// writes to `$HOME` may land in the workspace.
    #[test]
    fn the_agent_home_is_outside_the_workspace() {
        let sec = SecurityConfig::default();
        let plan = plan_with(&sec, &[], &host()).unwrap();

        let home = env_of(&plan, "HOME").expect("the sandbox must set HOME");
        assert_eq!(home, AGENT_HOME);
        assert!(
            !Path::new(home).starts_with(&plan.workdir),
            "HOME {home} must not be inside the workdir {}",
            plan.workdir
        );

        // It is writable, and backed by the host directory the caller supplied.
        let bind = plan
            .binds
            .iter()
            .find(|b| b.target == AGENT_HOME)
            .expect("HOME must be bound");
        assert_eq!(bind.mode, BindMode::ReadWrite);
        assert_eq!(bind.source, Path::new("/cache/cowboy/home/proj"));
        // Landlock is derived from the bind, so the write rule follows it.
        assert!(plan
            .landlock
            .read_write
            .contains(&PathBuf::from(AGENT_HOME)));
    }

    /// The host's mise store is shared copy-on-write, never as a plain bind.
    ///
    /// Read-only would break the `mise install` Cowboy runs at session start the
    /// moment a project pins a version the host lacks; read-write would let the
    /// agent rewrite a binary the user later runs on the host. The overlay is the
    /// only shape that is both usable and safe, so assert it *is* an overlay and
    /// that the store never appears in the bind list under either mode.
    #[test]
    fn the_hosts_mise_store_is_shared_copy_on_write() {
        let sec = SecurityConfig::default();
        // The project must look like a mise project, or the overlay is (correctly)
        // suppressed — see `project_uses_mise`.
        let host = host().with_existing(["/home/dev/.local/share/mise", "/srv/proj/mise.toml"]);
        let plan = plan_with(&sec, &[], &host).unwrap();

        let o = plan
            .overlays
            .iter()
            .find(|o| o.lower == Path::new("/home/dev/.local/share/mise"))
            .expect("the mise store should be shared");
        // Exposed at its host path: installs are full of absolute shebangs.
        assert_eq!(o.target, "/home/dev/.local/share/mise");
        // Writes must land with the project, not in the user's store…
        assert!(o.upper.starts_with("/srv/proj"), "upper: {:?}", o.upper);
        assert!(o.work.starts_with("/srv/proj"), "work: {:?}", o.work);
        assert_ne!(o.upper, o.work, "overlayfs needs a separate workdir");
        // Both under one parent, which carries the `.gitignore` that keeps a
        // multi-gigabyte store out of `git status`.
        assert_eq!(o.upper.parent(), o.work.parent());
        assert!(o
            .upper
            .starts_with(Path::new("/srv/proj").join(MISE_OVERLAY_DIR)));
        // …and the store must never be a bind, in either mode.
        assert!(
            !plan
                .binds
                .iter()
                .any(|b| b.source == Path::new("/home/dev/.local/share/mise")),
            "the mise store must be an overlay, never a bind: {:?}",
            plan.binds
        );
        // The tool must be pointed at it, or the overlay is invisible.
        assert_eq!(
            env_of(&plan, "MISE_DATA_DIR"),
            Some("/home/dev/.local/share/mise")
        );
        // …and Landlock must grant it, or the mount is inert.
        //
        // This is the regression that made `share_mise_store` silently useless in a
        // real project: Landlock rules were derived from the bind list alone, and an
        // overlay is not a bind, so the target sat outside the ruleset. bwrap mounted
        // it read-write and the upperdir was writable, yet readdir, read and write
        // were all denied — surfacing as `mise install` dying with `Permission denied
        // (os error 13)` at session start. `stat` kept working (Landlock has no stat
        // access right), which is what made it read as a broken mount rather than a
        // missing rule.
        assert!(
            plan.landlock.read_write.contains(&PathBuf::from(&o.target)),
            "the overlay target needs a read-write Landlock rule or every access to \
             it is denied; rw rules: {:?}",
            plan.landlock.read_write
        );
        assert!(
            !plan.landlock.read_only.contains(&PathBuf::from(&o.target)),
            "a read-only rule would block the very installs this overlay exists for"
        );
    }

    /// A project that does not use mise gets no overlay, so no `.cowboy/mise/`
    /// directory is created in it and nothing is mounted that it has no use for.
    ///
    /// RECONSTRUCTED after an accidental `git checkout` of this file — see the note on
    /// [`PROJECT_MISE_CONFIGS`].
    #[test]
    fn a_non_mise_project_gets_no_mise_overlay() {
        let sec = SecurityConfig::default();
        // Host store present, but the project root carries none of the mise config
        // files, so there is nothing to install and no reason for the overlay.
        let host = host().with_existing(["/home/dev/.local/share/mise"]);
        let plan = plan_with(&sec, &[], &host).unwrap();
        assert!(
            plan.overlays.is_empty(),
            "a non-mise project must not get a mise overlay: {:?}",
            plan.overlays
        );
        assert_eq!(env_of(&plan, "MISE_DATA_DIR"), None);
    }

    /// Every configured location counts as "this project uses mise".
    ///
    /// RECONSTRUCTED: the lost changes included a second test I could not recover. This
    /// covers the gap it most likely filled — each entry in [`PROJECT_MISE_CONFIGS`]
    /// being recognised — since a typo'd or dropped entry silently costs a project its
    /// shared toolchain store with no error anywhere.
    #[test]
    fn every_mise_config_location_is_recognised() {
        for cfg in PROJECT_MISE_CONFIGS {
            let host = host().with_existing([
                "/home/dev/.local/share/mise",
                // Leaked as a String so the fixture can take a `&'static str`-ish
                // borrow; this is a test, and the list is tiny.
                Box::leak(format!("/srv/proj/{cfg}").into_boxed_str()) as &str,
            ]);
            let plan = plan_with(&SecurityConfig::default(), &[], &host).unwrap();
            assert!(
                !plan.overlays.is_empty(),
                "a project configured with {cfg:?} should get the mise overlay"
            );
        }
    }

    /// Both switches must actually switch it off, and a host without mise must not
    /// produce an overlay of a directory that is not there.
    #[test]
    fn sharing_the_mise_store_can_be_turned_off() {
        let with_mise =
            host().with_existing(["/home/dev/.local/share/mise", "/srv/proj/mise.toml"]);

        let off = SecurityConfig {
            sandbox: cowboy_core::config::SandboxConfig {
                share_mise_store: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let plan = plan_with(&off, &[], &with_mise).unwrap();
        assert!(plan.overlays.is_empty());
        assert_eq!(env_of(&plan, "MISE_DATA_DIR"), None);

        // `host_tools: false` means the machine's toolchain is not exposed at all,
        // and the mise store is part of that toolchain.
        let no_tools = SecurityConfig {
            sandbox: cowboy_core::config::SandboxConfig {
                host_tools: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let plan = plan_with(&no_tools, &[], &with_mise).unwrap();
        assert!(plan.overlays.is_empty(), "host_tools off must cover it too");

        // No mise on the host: nothing to share, and no overlay of a missing dir.
        let plan = plan_with(&SecurityConfig::default(), &[], &host()).unwrap();
        assert!(plan.overlays.is_empty());
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
        let denylist = Denylist::build_for(&probe, Path::new("/srv/proj"), Platform::Linux);
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
            assert!(
                bind.required,
                "the mask must be a required (non-try) bind: a skipped mask leaves {f} exposed"
            );
        }
    }

    /// The Ranch store (committed, user-gated scope) is bound read-only into the
    /// sandbox when present, so a sandboxed command can read its brief but cannot
    /// rewrite `ranch.yaml` to smuggle a scope change past the propose→approve gate.
    #[test]
    fn the_ranch_store_is_bound_read_only() {
        let probe = host().with_existing(["/srv/proj/.cowboy/ranches"]);
        let sec = SecurityConfig::default();
        let plan = plan_with(&sec, &[], &probe).unwrap();
        let target = format!("{}/.cowboy/ranches", sec.sandbox.workdir);
        let bind = plan
            .binds
            .iter()
            .find(|b| b.target == target)
            .expect("the ranch store must be bound when it exists");
        assert_eq!(bind.source, Path::new("/srv/proj/.cowboy/ranches"));
        assert_eq!(
            bind.mode,
            BindMode::ReadOnly,
            "the ranch store must be read-only"
        );
        // It appears in Landlock's read-only set, so the confinement holds even if
        // the bind were wrong (defence in depth).
        assert!(
            plan.landlock.read_only.contains(&PathBuf::from(&target)),
            "the ranch store must be Landlock read-only too"
        );
        // And it does not appear as writable.
        assert!(!plan.landlock.read_write.contains(&PathBuf::from(&target)));
    }

    /// Absent ranch store → no bind (nothing to protect, and bwrap would refuse a
    /// missing source).
    #[test]
    fn no_ranch_bind_when_the_store_is_absent() {
        let sec = SecurityConfig::default();
        let plan = plan_with(&sec, &[], &host()).unwrap();
        let target = format!("{}/.cowboy/ranches", sec.sandbox.workdir);
        assert!(!plan.binds.iter().any(|b| b.target == target));
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

    /// A runtime grant whose path is (or passes through) a symlink to a credential
    /// store must be refused: the denylist match is lexical, so like configured
    /// mounts, grants are canonicalized first (`denied_source`) — otherwise a grant
    /// for a benign dir could be swapped for a symlink to `~/.aws` and `bwrap --bind`
    /// would follow it (M1).
    #[test]
    fn refuses_a_grant_that_symlinks_into_a_credential_store() {
        let host = host().with_symlink("/srv/other/looks-fine", "/home/dev/.aws");
        let grants = [Grant {
            path: PathBuf::from("/srv/other/looks-fine"),
            read_only: true,
        }];
        let err = plan_with(&SecurityConfig::default(), &grants, &host)
            .expect_err("a grant symlinked to a credential store must be refused");
        assert!(matches!(err, Error::SecurityInvariant(_)), "{err:?}");
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
    fn a_mount_source_that_symlinks_into_a_credential_store_is_refused() {
        // A benign-looking source inside the project that is actually a symlink to
        // the user's AWS credentials. The denylist's own matching is lexical, so
        // this is caught only because the plan canonicalizes the source first
        // (finding 2). Without that, `/srv/proj/innocent` sails through.
        let host = host().with_symlink("/srv/proj/innocent", "/home/dev/.aws");
        let mut sec = SecurityConfig::default();
        sec.sandbox.mounts.push(Mount {
            source: "/srv/proj/innocent".into(),
            target: "/workspace/innocent".into(),
            mode: "ro".into(),
        });
        let err = plan_with(&sec, &[], &host)
            .expect_err("a mount source symlinked to a credential store must be refused");
        assert!(matches!(err, Error::SecurityInvariant(_)), "{err:?}");
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
        let rendered = plan.render(&Denylist::build_for(
            &host(),
            Path::new("/srv/proj"),
            Platform::Linux,
        ));
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
        let denylist = Denylist::build_for(&probe, Path::new("/srv/proj"), Platform::Linux);
        insta::assert_snapshot!(plan.render(&denylist));
    }

    // ---- macOS: the same plan, rendered for Seatbelt ------------------------------

    fn mac_host() -> FakeHost {
        FakeHost {
            developer_bundle: Some(PathBuf::from("/Applications/Xcode.app")),
            darwin_user_temp: Some(PathBuf::from("/private/var/folders/xy/abc/T")),
            ..FakeHost::new().with_home("/Users/dev").with_existing([
                "/usr",
                "/System",
                "/opt/homebrew",
                "/Applications/Xcode.app",
                "/Users/dev/proj",
                "/Users/dev/.aws",
                "/Users/dev/.cargo/bin",
            ])
        }
    }

    fn mac_plan(security: &SecurityConfig, grants: &[Grant]) -> Result<SandboxPlan> {
        let root = Path::new("/Users/dev/proj");
        SandboxPlan::build(
            &PlanInputs {
                platform: Platform::MacOs,
                scratch: Path::new("/Users/dev/.cache/cowboy/run/scratch/s"),
                agent_home: Path::new("/Users/dev/.cache/cowboy/home/proj"),
                ..inputs(root, security, grants, Path::new("/unused/mask"))
            },
            &mac_host(),
        )
    }

    /// Nothing can be remapped, so the project is where it is, and a config written
    /// for Linux (`/workspace`) still works — with a note saying what was ignored.
    #[test]
    fn on_macos_the_project_is_at_its_host_path() {
        let plan = mac_plan(&SecurityConfig::default(), &[]).unwrap();
        assert_eq!(plan.workdir, "/Users/dev/proj");
        let project = plan
            .binds
            .iter()
            .find(|b| b.why == "the project")
            .expect("the project is exposed");
        assert_eq!(project.target, "/Users/dev/proj");
        assert_eq!(project.mode, BindMode::ReadWrite);
        assert!(
            plan.notes.iter().any(|n| n.contains("/workspace")),
            "{:?}",
            plan.notes
        );
        for b in &plan.binds {
            assert_eq!(
                Path::new(&b.target),
                b.source,
                "every exposure is at its own path on macOS: {b:?}"
            );
        }
    }

    /// The config mask is a deny rule, present whether or not the file exists yet —
    /// so the agent cannot create one for the host to read later — and there is no
    /// Linux-style mask bind left over.
    #[test]
    fn on_macos_host_owned_config_is_masked_by_path() {
        let plan = mac_plan(&SecurityConfig::default(), &[]).unwrap();
        assert_eq!(
            plan.masks,
            vec![
                PathBuf::from("/Users/dev/proj/.cowboy/security.yaml"),
                PathBuf::from("/Users/dev/proj/.cowboy/models.yaml"),
            ]
        );
        assert!(!plan.binds.iter().any(|b| b.required), "{:?}", plan.binds);
    }

    #[test]
    fn on_macos_home_and_tmpdir_are_the_agents_own() {
        let plan = mac_plan(&SecurityConfig::default(), &[]).unwrap();
        assert_eq!(
            env_of(&plan, "HOME"),
            Some("/Users/dev/.cache/cowboy/home/proj")
        );
        assert_eq!(
            env_of(&plan, "TMPDIR"),
            Some("/Users/dev/.cache/cowboy/run/scratch/s/tmp")
        );
        assert_eq!(env_of(&plan, "GIT_HTTP_PROXY_AUTHMETHOD"), Some("basic"));
        assert!(
            env_of(&plan, "PATH").unwrap().contains("/opt/homebrew/bin"),
            "Homebrew is where the user's tools resolve"
        );
        // Only `tmp`: `/run` and `/var/tmp` exist to be remapped, which macOS cannot.
        let scratch: Vec<_> = plan
            .binds
            .iter()
            .filter(|b| b.source.starts_with("/Users/dev/.cache/cowboy/run"))
            .collect();
        assert_eq!(scratch.len(), 1, "{scratch:?}");
    }

    /// A credential grant whose tool looks in `~` is linked into the agent's HOME,
    /// including the presets' legacy `/tmp/…` targets.
    #[test]
    fn on_macos_a_credential_grant_is_linked_into_home() {
        let mut sec = SecurityConfig::default();
        sec.secrets.files.push(cowboy_core::config::SecretMount {
            source: "~/.aws".into(),
            target: "/tmp/.aws".into(),
            read_only: true,
            required: false,
            approval: None,
        });
        let plan = mac_plan(&sec, &[]).unwrap();
        assert_eq!(
            plan.home_links,
            vec![(
                PathBuf::from("/Users/dev/.cache/cowboy/home/proj/.aws"),
                PathBuf::from("/Users/dev/.aws")
            )]
        );
        let b = plan
            .binds
            .iter()
            .find(|b| b.source == Path::new("/Users/dev/.aws"))
            .unwrap();
        assert_eq!(b.mode, BindMode::ReadOnly);
    }

    /// The denylist is platform-independent: a runtime grant for a credential store
    /// is refused on macOS exactly as on Linux.
    #[test]
    fn on_macos_a_denylisted_grant_is_still_refused() {
        let grants = [Grant {
            path: PathBuf::from("/Users/dev/.aws"),
            read_only: true,
        }];
        let err = mac_plan(&SecurityConfig::default(), &grants).unwrap_err();
        assert!(err.to_string().contains("refused"), "{err}");
    }

    #[test]
    fn on_macos_nothing_linux_specific_is_planned() {
        let plan = mac_plan(&SecurityConfig::default(), &[]).unwrap();
        assert!(plan.overlays.is_empty());
        assert!(plan.symlinks.is_empty());
        assert!(plan.proc_at.is_empty() && plan.dev_at.is_empty());
        assert!(plan.landlock.read_only.is_empty() && plan.seccomp.denied.is_empty());
        assert_eq!(
            plan.xcrun_cache_prefix,
            Some(PathBuf::from("/private/var/folders/xy/abc/T/xcrun_db"))
        );
        assert!(plan
            .binds
            .iter()
            .any(|b| b.source == Path::new("/Applications/Xcode.app")));
    }

    #[test]
    fn macos_plan_snapshot() {
        let plan = mac_plan(&SecurityConfig::default(), &[]).unwrap();
        let denylist =
            Denylist::build_for(&mac_host(), Path::new("/Users/dev/proj"), Platform::MacOs);
        insta::assert_snapshot!(plan.render(&denylist));
    }
}
