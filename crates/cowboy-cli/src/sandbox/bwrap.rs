//! Turn a [`SandboxPlan`] into a `bwrap` invocation.
//!
//! We use bubblewrap for the mount namespace, the bind set, and `pivot_root`
//! rather than hand-rolling them. That sequencing is a well-known source of
//! sandbox escapes and bwrap's version has been audited for a decade; there is
//! nothing to gain by rewriting it. What bwrap *cannot* do — apply Landlock — is
//! done by the shim it execs (see [`super::shim`]).
//!
//! bwrap must **not** be setuid. A setuid helper is a different threat model
//! entirely: it runs as root and its argument handling becomes attack surface,
//! whereas the unprivileged-user-namespace path grants no privilege we did not
//! already have. `cowboy doctor` rejects a setuid bwrap.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use cowboy_sandbox::{BindMode, SandboxPlan};

/// Where bwrap is expected. Resolved once and checked, so a setuid binary or a
/// missing one is a clear error rather than a confusing failure deep in a command.
pub fn resolve_bwrap() -> Result<PathBuf> {
    let path = which_bwrap().context(
        "bubblewrap (bwrap) is required for the sandbox but was not found on PATH. \
         Install it (Gentoo: emerge sys-apps/bubblewrap).",
    )?;
    ensure_not_setuid(&path)?;
    Ok(path)
}

fn which_bwrap() -> Option<PathBuf> {
    // An explicit override first, for testing and unusual layouts.
    if let Some(p) = std::env::var_os("COWBOY_BWRAP").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join("bwrap"))
            .find(|p| p.is_file())
    })
}

/// Refuse a setuid (or setgid) bwrap.
///
/// Distro packages sometimes ship it setuid root so it works where unprivileged
/// user namespaces are disabled. We rely on unprivileged userns instead, and
/// running a setuid-root helper on every command would widen the boundary rather
/// than enforce it — so this is a hard error, not a warning.
pub fn ensure_not_setuid(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let md = std::fs::metadata(path).with_context(|| format!("cannot stat {}", path.display()))?;
    let mode = md.permissions().mode();
    if mode & 0o4000 != 0 || mode & 0o2000 != 0 {
        bail!(
            "{} is setuid/setgid (mode {:o}). Cowboy requires an unprivileged \
             bubblewrap: a setuid-root helper running on every agent command widens \
             the boundary instead of enforcing it. Rebuild bubblewrap without the \
             suid USE flag (Gentoo: USE=\"-suid\" emerge sys-apps/bubblewrap).",
            path.display(),
            mode & 0o7777
        );
    }
    Ok(())
}

/// How the sandbox should treat the network namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetMode {
    /// Unshare a fresh namespace with only loopback. No egress at all, because
    /// there is no host-connected device — used before the egress transport exists
    /// and by tests that must not touch the network.
    Isolated,
    /// Inherit the caller's namespace. The caller is expected to have already
    /// entered the session network namespace, where the transport is installed.
    Inherit,
}

/// Build the full argv: `bwrap <options> -- <shim> <command>`.
///
/// Order is significant and mirrors the plan's: binds are applied in sequence and
/// a later one shadows an earlier one, which is precisely how the host-owned config
/// mask works. Reordering this silently weakens the boundary.
pub fn build_argv(
    bwrap: &Path,
    plan: &SandboxPlan,
    net: NetMode,
    shim_argv: &[OsString],
) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec![bwrap.into()];
    // A macro rather than a closure: a closure would hold a mutable borrow of `a`
    // for the whole function, blocking the interleaved `a.push` calls for path
    // arguments.
    macro_rules! push {
        ($s:expr) => {
            a.push(OsString::from($s))
        };
    }

    // Namespaces. A fresh user namespace is what empties the capability bounding
    // set: `--cap-drop ALL` alone clears the effective set but leaves the bounding
    // set intact, so a capability could in principle be regained. With a nested
    // user namespace both are empty.
    push!("--unshare-user");
    push!("--unshare-ipc");
    push!("--unshare-pid");
    push!("--unshare-uts");
    push!("--unshare-cgroup-try");
    if net == NetMode::Isolated {
        push!("--unshare-net");
    }
    push!("--cap-drop");
    push!("ALL");
    // `--die-with-parent` is load-bearing, not tidiness. bwrap keeps itself as a
    // monitor outside the new PID namespace and forks the actual PID 1, so killing
    // the monitor alone orphans the sandbox: verified to leave a re-`setsid`'d
    // grandchild running. With this flag, killing the monitor takes down PID 1 and
    // the kernel reaps the entire namespace — which is why no pidfile or
    // `/proc`-sweep is needed here, unlike the Docker exec path.
    push!("--die-with-parent");
    // Own session so the sandbox cannot reach the caller's terminal (e.g. inject
    // input with TIOCSTI on kernels that still allow it).
    push!("--new-session");

    push!("--hostname");
    push!("cowboy");

    for (target, link) in &plan.symlinks {
        push!("--symlink");
        a.push(target.into());
        a.push(link.into());
    }

    // Special filesystems FIRST, so a bind can be made *inside* one of them.
    //
    // This ordering is load-bearing and was originally the other way round. With the
    // special filesystems mounted after the binds, a grant for a path under /tmp (a
    // temporary directory, say) was silently shadowed: it appeared in
    // `cowboy sandbox plan` and then did not exist inside the sandbox. The plan
    // separately refuses any bind that would shadow /proc or /dev, so nothing is
    // lost by no longer relying on order for that.
    push!("--proc");
    a.push(plan.proc_at.clone().into());
    push!("--dev");
    a.push(plan.dev_at.clone().into());

    for b in &plan.binds {
        // `try` variants: an optional path that vanished between planning and now
        // should not abort the command. But a bind marked `required` (the config
        // mask) is the opposite case — its *absence* would widen the boundary (an
        // unmasked `security.yaml`), so use the non-`try` bind, which aborts the
        // spawn if the source is missing. Fail closed on the one bind where a silent
        // skip is dangerous.
        match (b.mode, b.required) {
            (BindMode::ReadOnly, true) => push!("--ro-bind"),
            (BindMode::ReadOnly, false) => push!("--ro-bind-try"),
            (BindMode::ReadWrite, true) => push!("--bind"),
            (BindMode::ReadWrite, false) => push!("--bind-try"),
        }
        a.push(b.source.clone().into_os_string());
        a.push(b.target.clone().into());
    }

    // Overlays AFTER the binds: an overlay's target can be a path a bind created
    // (a tool store under a bound home), and bwrap applies mounts in argument
    // order. Each `--overlay-src` names a lower layer for the `--overlay` that
    // follows it; writes land in the upper dir, so the host's copy is read-only in
    // effect without being read-only in the mount — which is the whole point.
    for o in &plan.overlays {
        push!("--overlay-src");
        a.push(o.lower.clone().into_os_string());
        push!("--overlay");
        a.push(o.upper.clone().into_os_string());
        a.push(o.work.clone().into_os_string());
        a.push(o.target.clone().into());
    }

    push!("--chdir");
    a.push(plan.workdir.clone().into());

    // Make the sandbox root read-only, LAST so it applies after every mount.
    //
    // bwrap builds the new root on a tmpfs and auto-creates directories for bind
    // targets, so `/`, `/etc` and `/var` are writable by default even though the
    // binds *inside* them are read-only. That would let the agent drop an
    // `/etc/ld.so.preload`. Separate mounts keep their own flags, so `/workspace`,
    // `/tmp` and `/run` stay writable — verified, not assumed.
    push!("--remount-ro");
    push!("/");

    // A clean environment: the host's variables would otherwise leak tokens and
    // paths into the sandbox wholesale.
    push!("--clearenv");
    for (k, v) in &plan.env {
        push!("--setenv");
        a.push(k.clone().into());
        a.push(v.clone().into());
    }

    push!("--");
    a.extend(shim_argv.iter().cloned());
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use cowboy_core::config::SecurityConfig;
    use cowboy_sandbox::plan::{PlanInputs, SandboxPlan};
    use cowboy_sandbox::probe::FakeHost;

    fn plan() -> SandboxPlan {
        let probe =
            FakeHost::new().with_existing(["/usr", "/srv/proj", "/srv/proj/.cowboy/security.yaml"]);
        let sec = SecurityConfig::default();
        let inputs = PlanInputs {
            root: Path::new("/srv/proj"),
            security: &sec,
            grants: &[],
            mask_file: Path::new("/run/mask"),
            relay_port: 8443,
            scratch: Path::new("/scratch"),
            agent_home: Path::new("/cache/cowboy/home/proj"),
            git_identity: None,
        };
        SandboxPlan::build(&inputs, &probe).unwrap()
    }

    /// An overlay must be applied after the binds (its target can be a path a bind
    /// created) and before `--remount-ro /`, and `--overlay-src` must immediately
    /// precede the `--overlay` it feeds — bwrap reads them positionally, so a
    /// reordering silently changes which lower layer is used.
    #[test]
    fn an_overlay_is_applied_after_the_binds_and_reads_its_source_first() {
        let mut p = plan();
        p.overlays = vec![cowboy_sandbox::plan::Overlay {
            lower: "/home/dev/.local/share/mise".into(),
            upper: "/srv/proj/.cowboy/mise/upper".into(),
            work: "/srv/proj/.cowboy/mise/work".into(),
            target: "/home/dev/.local/share/mise".into(),
            why: "test".into(),
        }];
        let out: Vec<String> = build_argv(Path::new("/usr/bin/bwrap"), &p, NetMode::Isolated, &[])
            .into_iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();

        let src = out
            .iter()
            .position(|a| a == "--overlay-src")
            .expect("a lower layer");
        let ovl = out
            .iter()
            .position(|a| a == "--overlay")
            .expect("an overlay");
        assert_eq!(out[src + 1], "/home/dev/.local/share/mise", "lower layer");
        assert_eq!(ovl, src + 2, "--overlay must follow its --overlay-src");
        assert_eq!(
            out[ovl + 1],
            "/srv/proj/.cowboy/mise/upper",
            "writes go to upper"
        );
        assert_eq!(out[ovl + 2], "/srv/proj/.cowboy/mise/work");
        assert_eq!(
            out[ovl + 3],
            "/home/dev/.local/share/mise",
            "merged at the host path"
        );

        let last_bind = out
            .iter()
            .rposition(|a| a.starts_with("--ro-bind") || a.starts_with("--bind"))
            .expect("some binds");
        assert!(src > last_bind, "overlays come after every bind");
        let remount = out.iter().position(|a| a == "--remount-ro").unwrap();
        assert!(ovl < remount, "and before the root is sealed");
    }

    fn argv_strings(net: NetMode) -> Vec<String> {
        build_argv(
            Path::new("/usr/bin/bwrap"),
            &plan(),
            net,
            &[
                OsString::from("/usr/bin/cowboy"),
                OsString::from("x-sandbox-shim"),
            ],
        )
        .into_iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect()
    }

    #[test]
    fn unshares_namespaces_and_drops_capabilities() {
        let a = argv_strings(NetMode::Isolated);
        for flag in [
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-net",
            "--new-session",
        ] {
            assert!(a.contains(&flag.to_string()), "missing {flag}");
        }
        let i = a.iter().position(|s| s == "--cap-drop").unwrap();
        assert_eq!(a[i + 1], "ALL");
    }

    /// Without this, killing bwrap orphans the sandbox instead of reaping it.
    #[test]
    fn always_dies_with_parent() {
        assert!(argv_strings(NetMode::Isolated).contains(&"--die-with-parent".to_string()));
        assert!(argv_strings(NetMode::Inherit).contains(&"--die-with-parent".to_string()));
    }

    /// Inherit mode must not unshare the network: the caller has already entered
    /// the session namespace where the transport is installed, and unsharing here
    /// would discard it (leaving no egress at all).
    #[test]
    fn inherit_mode_does_not_unshare_the_network() {
        assert!(!argv_strings(NetMode::Inherit).contains(&"--unshare-net".to_string()));
    }

    /// The mask relies on later binds shadowing earlier ones, so plan order must
    /// survive into the argv unchanged.
    #[test]
    fn bind_order_is_preserved_with_the_mask_last() {
        let a = argv_strings(NetMode::Isolated);
        let mask = a
            .iter()
            .position(|s| s == "/run/mask")
            .expect("mask bind present");
        let proj = a
            .iter()
            .position(|s| s == "/srv/proj")
            .expect("project bind present");
        assert!(proj < mask, "the mask must come after the project bind");
    }

    /// The special filesystems come BEFORE the binds, so a bind under one of them
    /// is mounted inside it rather than shadowed by it. With the old order the
    /// order shadowed a grant under /tmp: it showed up in `cowboy sandbox plan`
    /// and then did not exist inside the sandbox. The plan refuses binds that
    /// would shadow /proc or /dev, so nothing relies on order for that.
    #[test]
    fn special_filesystems_are_mounted_before_binds() {
        let a = argv_strings(NetMode::Isolated);
        let first_bind = a
            .iter()
            .position(|s| s == "--ro-bind-try" || s == "--bind-try")
            .unwrap();
        for flag in ["--proc", "--dev"] {
            let i = a.iter().position(|s| s == flag).unwrap();
            assert!(i < first_bind, "{flag} must precede the binds");
        }
    }

    /// Without this the sandbox root tmpfs is writable, letting the agent create
    /// `/etc/ld.so.preload`. It must come after every mount to apply at all.
    #[test]
    fn root_is_remounted_read_only_after_all_mounts() {
        let a = argv_strings(NetMode::Isolated);
        let i = a
            .iter()
            .position(|s| s == "--remount-ro")
            .expect("the sandbox root must be remounted read-only");
        assert_eq!(a[i + 1], "/");
        let last_mount = a
            .iter()
            .rposition(|s| {
                s == "--ro-bind-try" || s == "--bind-try" || s == "--dev" || s == "--proc"
            })
            .unwrap();
        assert!(i > last_mount, "--remount-ro / must come after every mount");
    }

    /// The host environment must not leak in wholesale.
    #[test]
    fn environment_is_cleared_then_set_explicitly() {
        let a = argv_strings(NetMode::Isolated);
        let clear = a.iter().position(|s| s == "--clearenv").unwrap();
        let first_setenv = a.iter().position(|s| s == "--setenv").unwrap();
        assert!(clear < first_setenv, "--clearenv must precede --setenv");
    }

    #[test]
    fn shim_argv_comes_after_the_separator() {
        let a = argv_strings(NetMode::Isolated);
        let sep = a.iter().position(|s| s == "--").unwrap();
        assert_eq!(&a[sep + 1..], &["/usr/bin/cowboy", "x-sandbox-shim"]);
    }

    #[test]
    fn setuid_bwrap_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("cowboy-suid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("bwrap");
        std::fs::write(&fake, b"#!/bin/sh\n").unwrap();

        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ensure_not_setuid(&fake).is_ok(), "plain 0755 is fine");

        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o4755)).unwrap();
        let err = ensure_not_setuid(&fake).expect_err("setuid must be refused");
        assert!(err.to_string().contains("setuid"), "{err}");

        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o2755)).unwrap();
        assert!(
            ensure_not_setuid(&fake).is_err(),
            "setgid must be refused too"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
