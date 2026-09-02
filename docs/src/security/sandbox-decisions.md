# Sandbox design decisions

Decision record for the host-native sandbox (namespaces + Landlock + seccomp),
which replaced the Docker-based isolation. Every claim here was verified by
running code on the target host, not reasoned from documentation. Re-verify
anything load-bearing before depending on it — the checks live in
`cowboy doctor`.

## Target host

```
kernel   7.2.0-gentoo-x86_64      (Landlock ABI >= 6 guaranteed; >= 6.12)
LSMs     lockdown,capability,landlock,yama,selinux,ima,evm
yama     ptrace_scope = 1         (descendants only)
bwrap    /usr/bin/bwrap, mode 0755 — NOT setuid
nft      v1.1.7
```

Other distributions are an explicit follow-up. `cowboy doctor` is the executable
specification of what a host must provide.

## Egress transport: A (nft DNAT), not B (tun + netstack)

Transport A was validated end to end inside an unprivileged user namespace:
`dnat` interception, `SO_ORIGINAL_DST` recovery of the true destination,
bidirectional data flow, and a `filter output` backstop dropping non-DNS UDP and
ICMP. Transport B (tun + a userspace TCP/IP stack) was therefore not built; the
`EgressTransport` seam keeps it available if a future host cannot run A.

Three findings changed the plan:

**`dnat` is used rather than `redirect`.** They are equivalent for our purposes —
`redirect` is shorthand for DNAT to a local address — but `dnat` needs only
`nft_nat`, which the nat chain requires anyway, whereas `redirect` additionally
needs `nft_redir` (`CONFIG_NFT_REDIR=m` here, and not loaded at boot). One fewer
dependency for identical behaviour, so no kernel reconfiguration was requested.

**nf_tables expression modules DO autoload from a user namespace.** The plan
assumed they could not, on the grounds that module loading needs
`CAP_SYS_MODULE` in the initial user namespace. That is true of `init_module`,
but nf_tables resolves a missing expression by calling `request_module()` from a
privileged kernel context, so `nft_redir` loaded on demand from inside the
sandbox during testing. This substantially weakens — but does not entirely
remove — the concern that the nat substrate is only present because Docker
loaded it. `doctor` still distinguishes "present but not loaded" from
"unavailable".

**A loopback-only netns cannot be intercepted.** `nat output` runs *after* the
routing decision, so `connect()` to an off-net address fails with `ENETUNREACH`
before nftables ever sees the packet. The sandbox therefore gets a **black-hole
device**: a veth pair (`CONFIG_VETH` is loaded; `CONFIG_DUMMY` is not), an
address, and a default route via a gateway that does not exist. Routing succeeds,
the nat hook fires, and the packet is redirected to the relay before it can go
anywhere. The device is deliberately connected to nothing.

### The `filter output` backstop needs both directions

The non-obvious part. After DNAT the agent's packet has
`saddr=<sandbox>, daddr=127.0.0.1`, but the relay's **reply** has
`saddr=127.0.0.1, daddr=<sandbox>` — and it is also locally generated output, so
it traverses the same chain. A backstop accepting only `ip daddr 127.0.0.0/8`
drops every SYN-ACK and the handshake times out, which presents as "interception
silently doesn't work". Both directions must be accepted:

```
chain filt {
  type filter hook output priority filter; policy drop;
  ip daddr 127.0.0.0/8 accept   # agent -> relay (post-DNAT)
  ip saddr 127.0.0.0/8 accept   # relay -> agent (replies)
}
```

`oifname "lo" accept` does **not** work as a substitute: the output interface is
decided before the DNAT, so it is still the black-hole device at the filter hook.

## Containment does not depend on the ruleset

The property that justifies the rewrite. The sandbox netns has no
host-connected device, so a total nftables failure yields **no egress** rather
than open egress. Under Docker the agent's netns had a real route out and the
ruleset was the only thing in the way, which meant an nft failure was a full
bypass — caught only because the gateway aborted startup and the runtime tore the
container down. nftables is now a *transparency* mechanism, not a *containment*
one.

## Per-command mount namespaces are built host-side; no `move_mount`

The host can `setns()` into a session sandbox's own user namespace and then its
network namespace, as the same uid, gaining full capabilities within it
(verified: both calls return 0, and `nft` works inside). Two consequences:

- Each command's mount namespace is constructed from the host, so a newly
  granted path is simply an entry in the next command's bind list.
- `open_tree`/`move_mount` fd injection is **not needed**. It would only have
  been required to inject mounts into an *existing* mount namespace under a
  supervisor-fork topology, and that topology is unnecessary.

Note that a network-namespace-only join fails with `EPERM`; the user namespace
must be joined first to acquire `CAP_SYS_ADMIN` over it.

## Per-command sandbox uses a nested user namespace

`bwrap --cap-drop ALL` alone leaves the bounding set full
(`CapEff: 0`, `CapBnd: 000001ffffffffff`). Adding `--unshare-user` empties both
(`CapEff: 0`, `CapBnd: 0`), so no capability can ever be regained even in the
presence of a setuid binary. `NoNewPrivs` is set either way. `nft` is denied in
both configurations, but the empty bounding set is the stronger guarantee, and
`CAP_NET_ADMIN` reaching an agent command would let it rewrite the ruleset.

## `--die-with-parent` is load-bearing, not tidiness

bwrap keeps *itself* as a monitor outside the new PID namespace and forks the
actual PID 1. Killing the monitor therefore does **not** take down the sandbox:
measured directly, a `setsid`'d grandchild survived. With `--die-with-parent`,
killing the monitor propagates to PID 1 and the kernel reaps the whole namespace —
the same test then left nothing behind.

This is why the sandbox executor needs no equivalent of the Docker path's
machinery for this. There, the command's pgid had to be written to a file inside
the container and `/proc/*/environ` swept for a per-exec `COWBOY_EXEC_TAG` marker
to catch descendants that re-`setsid`ed out of the recorded group. Here, one signal
to bwrap is sufficient and complete.

## The sandbox root must be remounted read-only

bwrap builds the new root on a tmpfs and auto-creates directories for bind
targets, so `/`, `/etc` and `/var` are **writable** by default even when every bind
inside them is read-only. Verified by probing: `touch /etc/EVIL` succeeded. That
would let the agent drop an `/etc/ld.so.preload`.

`--remount-ro /` as the final mount operation fixes it, and separate mounts keep
their own flags, so `/workspace`, `/tmp` and `/run` stay writable. Confirmed by
probe: `/ /etc /usr /var /opt` read-only, `/run /workspace /tmp` writable, and the
read-only binds still readable.

`oifname`-style reasoning does not apply here, but the same lesson does: the
ordering is the mechanism. `--remount-ro /` placed before the binds would do
nothing.

## The shim needs a fixed bind path

bwrap cannot apply Landlock, so it execs the `cowboy` binary as a shim which does
(see `crates/cowboy-cli/src/sandbox/shim.rs`). The binary must therefore be
reachable *inside* the sandbox, and its host path generally is not: the project is
bound at `/workspace`, so a development build at `<project>/target/debug/cowboy`
does not exist at that path inside.

It is bound read-only at `/.cowboy-shim`. A fixed top-level path because `/run`
and `/tmp` are tmpfs mounted after the binds (deliberately, so nothing can shadow
them), and `/usr` is a read-only bind of the host's, so no mount point can be
created inside it.

The shim receives its instructions as JSON on **stdin**, not argv, because argv is
visible in `/proc/<pid>/cmdline` to anything that can see the process.

## Beware the vacuously-passing sandbox test

`crates/cowboy-cli/tests/sandbox_exec.rs` self-skips when bubblewrap or
unprivileged user namespaces are unavailable, so that `--ignored` stays safe to run
anywhere. The first version of its capability probe ran `/bin/true` in a sandbox
that bound only `/usr` — where `/bin` is a symlink that does not exist — so the
probe reported "unsupported" and **all ten tests skipped while reporting `ok`**.

Two safeguards now: the probe includes the merged-`/usr` symlinks it needs (without
`/lib64` the dynamic linker is missing and even `/usr/bin/true` cannot start), and
`COWBOY_SANDBOX_TESTS=required` turns a skip into a failure. The wall-clock time is
the tell — the suite takes ~2s when it runs and ~0.01s when it skips.

Relatedly, that file counts processes by reading `/proc` directly rather than
using `pgrep -f`, whose pattern also matches the shell that invoked it. Same trap
as `pkill -f cowboyd`, which is why the repo forbids it.

## SELinux is not a risk on this host

It appears in `/sys/kernel/security/lsm`, which was flagged as a risk during
planning, but there is no `selinuxfs` mounted and no userspace tooling: it is
compiled in and disabled at runtime. No AVC denials were produced by any sandbox
operation. The concern stands for other distributions — an AVC denial in the
mount or unix-socket path presents as a bwrap bug — so it stays in the follow-up
project's notes.

## Supporting controls for the relay trust boundary

The relay reports the original destination to the gateway, so that channel is the
real enforcement boundary (see `docs/src/security/model.md`). It is an
**anonymous `socketpair()`** inherited across fork — nameless in the filesystem
and in every abstract namespace, so it cannot be opened, connected to, or
enumerated; reaching it requires already holding the fd. Three independent
controls back that up:

- the relay runs in its own PID namespace, so no agent command can see it;
- agent commands have an empty capability bounding set;
- yama `ptrace_scope=1` permits ptrace only of descendants, and the relay is not
  a descendant of any agent command.

Deliberately **not** dependent on `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET`, so no
Landlock ABI feature is load-bearing for this boundary.
