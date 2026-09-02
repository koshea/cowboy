//! Kernel-level lockdown applied by the shim immediately before `exec`.
//!
//! Three mechanisms, in a deliberate order:
//!
//! 1. `PR_SET_NO_NEW_PRIVS` — required before an unprivileged process may install
//!    either of the other two, and independently stops a setuid binary conferring
//!    anything.
//! 2. **Landlock** — a kernel-enforced filesystem and TCP-port domain. This is not
//!    a restatement of the mount view; it is enforced against the *process*, so it
//!    still holds if a bind is wrong, it is inherited by every descendant, and it
//!    can only ever be narrowed. It also covers `io_uring` filesystem operations,
//!    which seccomp cannot see.
//! 3. **seccomp-bpf** — refuses syscalls that have no legitimate use from a build
//!    and would otherwise be escape or inspection primitives.
//!
//! Landlock goes before seccomp so the filter cannot interfere with installing it,
//! and both go before `exec` because a Landlock domain can never be widened —
//! there is no later point at which they could be applied.
//!
//! Everything here **fails closed**. An error means the command does not run,
//! because the alternative is executing untrusted code with less confinement than
//! was promised.

use anyhow::{bail, Context, Result};
use landlock::{
    path_beneath_rules, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus, Scope, ABI,
};

use super::shim::ShimRequest;

/// The Landlock ABI this sandbox requires.
///
/// A hard floor, not a best-effort target. ABI 6 (Linux 6.12) is the first with
/// scoping for abstract unix sockets and signals, and the target host is well past
/// it. Declaring it as a hard requirement means an unsupported kernel is a loud
/// failure rather than a silently weaker sandbox — the failure mode that makes
/// "best effort" dangerous in a security boundary.
///
/// Note the residual gap: accesses introduced *after* ABI 6 are not handled by this
/// ruleset and so are unrestricted. That is deliberate — pinning the ABI keeps
/// enforcement identical across kernel upgrades instead of silently tightening (and
/// breaking builds) or silently loosening.
const REQUIRED_ABI: ABI = ABI::V6;

/// `SOCK_RAW`, and the mask that isolates the socket type from flags like
/// `SOCK_CLOEXEC`/`SOCK_NONBLOCK` which are OR'd into the same argument.
const SOCK_TYPE_MASK: u64 = 0xf;

/// Apply the full lockdown described by `req`.
pub fn apply(req: &ShimRequest) -> Result<()> {
    // Cheap assertion, and a real one: bwrap should already have emptied both
    // capability sets via `--unshare-user --cap-drop ALL`. If a future change to the
    // argv silently stopped doing that, the agent would hold CAP_NET_ADMIN in its
    // user namespace and could rewrite the nftables ruleset that makes egress
    // policy work. Refuse rather than run.
    ensure_no_capabilities()?;

    set_no_new_privs().context("setting PR_SET_NO_NEW_PRIVS")?;
    apply_landlock(req).context("applying the Landlock domain")?;
    apply_seccomp(req).context("installing the seccomp filter")?;
    Ok(())
}

/// Refuse to continue if the process holds any capability.
fn ensure_no_capabilities() -> Result<()> {
    let status = std::fs::read_to_string("/proc/self/status")
        .context("reading /proc/self/status to verify capabilities were dropped")?;
    for line in status.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        // Both sets matter. `CapEff` alone being empty is not enough: a non-empty
        // bounding set means a capability could in principle be regained.
        if matches!(key, "CapEff" | "CapBnd") {
            let hex = value.trim();
            let bits = u64::from_str_radix(hex, 16).unwrap_or(u64::MAX);
            if bits != 0 {
                bail!(
                    "refusing to run: {key} is {hex}, expected 0. The sandbox must have no \
                     capabilities — with CAP_NET_ADMIN the agent could rewrite the egress \
                     ruleset. Check that bwrap is invoked with --unshare-user --cap-drop ALL."
                );
            }
        }
    }
    Ok(())
}

fn set_no_new_privs() -> Result<()> {
    // SAFETY: prctl with PR_SET_NO_NEW_PRIVS takes no pointers and cannot fail
    // other than by returning -1.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// Install the Landlock domain.
///
/// **Filesystem only, deliberately.** Landlock also gates TCP bind and connect, but
/// by *port*, not address — and that distinction makes it unusable here:
///
/// - Handling `BindTcp` with no allowed ports denies every `bind()`, which breaks
///   the dev servers declared in `agent.yaml` (verified: binding `127.0.0.1:3000`
///   returned EACCES).
/// - Handling `ConnectTcp` with only the relay port allowed would stop the agent
///   reaching a service it started itself on some other loopback port.
/// - Allowing enough ports to fix both leaves the rule enforcing nothing useful,
///   because Landlock cannot tell "my own dev server" from "the internet".
///
/// It would not buy anything either. The sandbox network namespace holds no
/// host-connected device, so every outbound connection is already forced through
/// the nftables redirect into the relay and its policy engine. Containment does not
/// depend on a port rule, so adding one would only cost functionality.
///
/// Read-only paths get read plus **execute**: the sandbox's whole point is to run
/// the host's toolchain, and without `Execute` nothing in `/usr` could be invoked.
fn apply_landlock(req: &ShimRequest) -> Result<()> {
    let ro = AccessFs::from_read(REQUIRED_ABI);
    let rw = AccessFs::from_all(REQUIRED_ABI);

    // Hard requirement: error out on a kernel that cannot honour this, rather than
    // quietly enforcing less than we claim.
    let mut ruleset = Ruleset::default().set_compatibility(CompatLevel::HardRequirement);
    let r = &mut ruleset;
    r.handle_access(rw).context("handling filesystem access")?;

    if req.scope_ipc {
        // Hardening only — no trust boundary depends on this. The relay channel is
        // an anonymous socketpair, which has no name in any namespace and so cannot
        // be reached by connecting to anything.
        r.scope(Scope::AbstractUnixSocket)
            .context("scoping abstract unix sockets")?;
        r.scope(Scope::Signal).context("scoping signals")?;
    }

    let mut created = ruleset.create().context("creating the Landlock ruleset")?;

    // A missing path here is a bug, not an expected condition: these are
    // sandbox-internal targets that bwrap has just created, and the plan already
    // skipped optional host paths that were absent. Silently dropping failures is
    // how the "Landlock rules used host paths" bug stayed hidden — every rule failed
    // to resolve inside the sandbox and the domain ended up allowing nothing, which
    // looked like working confinement.
    created = created
        .add_rules(path_beneath_rules(&req.read_only, ro))
        .context("adding read-only Landlock rules (paths are sandbox-internal)")?;
    created = created
        .add_rules(path_beneath_rules(&req.read_write, rw))
        .context("adding read-write Landlock rules (paths are sandbox-internal)")?;

    let status = created.restrict_self().context("restrict_self")?;
    // With HardRequirement a partial application should already have errored, but
    // check the status too: silently running under a non-enforcing ruleset is the
    // one outcome that must never happen.
    if status.ruleset == RulesetStatus::NotEnforced {
        bail!(
            "the Landlock domain was not enforced (kernel ABI < {:?}?). Refusing to run \
             the command unconfined.",
            REQUIRED_ABI
        );
    }
    Ok(())
}

/// Compile and install the seccomp filter.
///
/// Allow by default, deny by exception: an allowlist would need to enumerate every
/// syscall a compiler, linker, test runner and package manager might make, and a
/// missed entry breaks a build in a way that is very hard to attribute. The
/// deny-list targets primitives with no legitimate use from a build.
fn apply_seccomp(req: &ShimRequest) -> Result<()> {
    use seccompiler::{
        SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule,
    };

    let mut rules: Vec<(i64, Vec<SeccompRule>)> = Vec::new();

    for name in &req.deny_syscalls {
        match syscall_number(name) {
            // An empty rule vec means "match this syscall unconditionally".
            Some(nr) => rules.push((nr, vec![])),
            // An unknown name is a bug in our own table, not user input. Fail
            // rather than silently enforcing less than the plan describes.
            None => bail!(
                "no syscall number known for {name:?} on this architecture; \
                 refusing to install an incomplete seccomp filter"
            ),
        }
    }

    if req.deny_raw_sockets {
        // `socket(_, type, _)`: refuse SOCK_RAW. Masked so the flags that share this
        // argument (SOCK_CLOEXEC, SOCK_NONBLOCK) cannot be used to slip past an
        // exact comparison.
        let raw = SeccompCondition::new(
            1,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::MaskedEq(SOCK_TYPE_MASK),
            libc::SOCK_RAW as u64,
        )
        .context("building the SOCK_RAW condition")?;
        rules.push((
            libc::SYS_socket,
            vec![SeccompRule::new(vec![raw]).context("building the SOCK_RAW rule")?],
        ));
    }

    if rules.is_empty() {
        return Ok(());
    }

    // EPERM rather than killing the process: a denied syscall should surface as an
    // ordinary error the command can report, not a mysterious SIGSYS death that
    // looks like a crash in the user's build.
    let filter = SeccompFilter::new(
        rules.into_iter().collect(),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        target_arch(),
    )
    .context("building the seccomp filter")?;

    let program: seccompiler::BpfProgram = filter.try_into().context("compiling to BPF")?;
    seccompiler::apply_filter(&program).context("applying the BPF filter")?;
    Ok(())
}

fn target_arch() -> seccompiler::TargetArch {
    #[cfg(target_arch = "x86_64")]
    return seccompiler::TargetArch::x86_64;
    #[cfg(target_arch = "aarch64")]
    return seccompiler::TargetArch::aarch64;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    compile_error!("the sandbox seccomp filter needs a syscall table for this architecture");
}

/// Syscall name to number.
///
/// Hand-maintained rather than pulled from `libseccomp`, to avoid a C dependency.
/// The numbers were taken from this machine's `asm/unistd_64.h`, not from memory.
/// `libc::SYS_*` is used wherever the constant exists so the values are checked by
/// the libc crate rather than by us.
fn syscall_number(name: &str) -> Option<i64> {
    let nr = match name {
        // io_uring: the reason the deny-list exists at all. Operations are
        // submitted as ring entries, not syscalls, so IORING_OP_CONNECT and
        // IORING_OP_OPENAT never pass through a filter on connect/openat. Landlock
        // does cover io_uring for filesystem access, so file confinement holds
        // regardless — but the network and syscall halves would be bypassable
        // unless the ring is refused outright.
        "io_uring_setup" => libc::SYS_io_uring_setup,
        "io_uring_enter" => libc::SYS_io_uring_enter,
        "io_uring_register" => libc::SYS_io_uring_register,
        // Kernel module and kexec surface.
        "init_module" => libc::SYS_init_module,
        "finit_module" => libc::SYS_finit_module,
        "delete_module" => libc::SYS_delete_module,
        "kexec_load" => libc::SYS_kexec_load,
        "kexec_file_load" => libc::SYS_kexec_file_load,
        // Tracing and BPF.
        "bpf" => libc::SYS_bpf,
        "perf_event_open" => libc::SYS_perf_event_open,
        // Privileged host-wide operations.
        "pivot_root" => libc::SYS_pivot_root,
        "swapon" => libc::SYS_swapon,
        "swapoff" => libc::SYS_swapoff,
        "reboot" => libc::SYS_reboot,
        "settimeofday" => libc::SYS_settimeofday,
        "clock_settime" => libc::SYS_clock_settime,
        "clock_adjtime" => libc::SYS_clock_adjtime,
        "adjtimex" => libc::SYS_adjtimex,
        // Legacy or rarely-used interfaces with a poor security record.
        "uselib" => libc::SYS_uselib,
        "userfaultfd" => libc::SYS_userfaultfd,
        "personality" => libc::SYS_personality,
        // Process inspection. yama ptrace_scope=1 already limits ptrace to
        // descendants; denying it outright means an agent command cannot inspect or
        // manipulate even its own children's memory to work around the filter.
        "ptrace" => libc::SYS_ptrace,
        "process_vm_readv" => libc::SYS_process_vm_readv,
        "process_vm_writev" => libc::SYS_process_vm_writev,
        _ => return None,
    };
    Some(nr)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every name the plan's default profile denies must resolve to a number.
    /// Otherwise `apply_seccomp` refuses to install the filter and no command runs
    /// — so a typo in either list is a total outage, not a silent weakening.
    #[test]
    fn every_denied_syscall_in_the_default_profile_is_known() {
        let profile = cowboy_sandbox::SeccompProfile::default();
        for name in &profile.denied {
            assert!(
                syscall_number(name).is_some(),
                "{name} is in the default profile but has no syscall number"
            );
        }
    }

    #[test]
    fn unknown_syscall_names_are_rejected() {
        assert!(syscall_number("definitely_not_a_syscall").is_none());
    }

    /// Spot-check against the numbers in this machine's asm/unistd_64.h.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn syscall_numbers_match_the_kernel_headers() {
        for (name, expected) in [
            ("io_uring_setup", 425),
            ("io_uring_enter", 426),
            ("io_uring_register", 427),
            ("bpf", 321),
            ("ptrace", 101),
            ("userfaultfd", 323),
        ] {
            assert_eq!(syscall_number(name), Some(expected), "{name}");
        }
    }

    /// The mask must isolate the socket type from the flags OR'd into the same
    /// argument, or `SOCK_RAW | SOCK_CLOEXEC` would slip past.
    #[test]
    fn sock_raw_mask_ignores_socket_flags() {
        let raw_with_flags = (libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK) as u64;
        assert_eq!(raw_with_flags & SOCK_TYPE_MASK, libc::SOCK_RAW as u64);
        // And a stream socket must not match.
        assert_ne!(
            (libc::SOCK_STREAM as u64) & SOCK_TYPE_MASK,
            libc::SOCK_RAW as u64
        );
    }

    /// A best-effort Landlock domain would silently enforce less than promised, so
    /// the ABI floor is a constant the tests pin rather than a runtime discovery.
    #[test]
    fn the_required_abi_supports_scoping() {
        // Scoping arrived in ABI 6; anything lower would make `scope_ipc` a no-op.
        assert!(REQUIRED_ABI as i32 >= ABI::V6 as i32);
    }
}
