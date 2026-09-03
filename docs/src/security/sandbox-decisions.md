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

## Landlock rules must use sandbox paths, and this fails silently

The shim applies the Landlock domain from *inside* the finished sandbox, so its
paths are resolved in the sandbox's mount namespace. Deriving them from the
host-side bind **sources** is the obvious mistake and an almost invisible one:
`/usr` has the same path inside and out, so reading the toolchain keeps working,
while the project (`/srv/x` outside, `/workspace` inside) gets no rule at all and
every write is denied. The rules must come from the bind **targets**.

Two things made it worse and are now fixed:

- The special filesystems are not binds. Rules derived from the bind list alone
  leave `/proc`, `/dev` and the tmpfs mounts outside the domain, so anything
  reading `/proc/self/*` fails.
- The rule-adding code originally dropped failed rules with
  `filter_map(|r| r.ok())`. Since every rule failed to resolve, the domain ended up
  with no allow rules — denying everything, which looks exactly like working
  confinement. Missing paths are now a hard error: they are sandbox-internal
  targets bwrap has just created, so absence is a bug, and the plan has already
  skipped optional host paths that were not present.

## Landlock does not gate the network here

Landlock gates TCP bind and connect by **port, not address**, which makes it
unusable for this sandbox:

- Handling `BindTcp` with no allowed ports denies every `bind()`. Verified:
  binding `127.0.0.1:3000` returned `EACCES`, which would break every dev server
  declared in `agent.yaml`.
- Handling `ConnectTcp` with only the relay port allowed would stop the agent
  reaching a service it started itself on any other loopback port.
- Allowing enough ports to fix both leaves the rule enforcing nothing meaningful,
  because a port number cannot distinguish "my own dev server" from "the internet".

It would also be redundant. The sandbox network namespace holds no host-connected
device, so every outbound connection is already forced through the nftables
redirect into the relay and the policy engine. Containment does not depend on a
port rule, so the only thing adding one achieves is breaking functionality.

Landlock is therefore **filesystem-only** here, plus ABI 6 scoping as hardening.
`cowboy sandbox plan` says so explicitly rather than listing a port allowlist that
does not exist.

## Why the ABI is a hard requirement

`CompatLevel::HardRequirement` with a pinned `ABI::V6`, not best-effort. A
best-effort domain on an older kernel would enforce a subset while reporting
success, which is the failure mode that makes "graceful degradation" dangerous in a
security boundary. `restrict_self`'s status is checked as well, so a non-enforcing
ruleset aborts the command.

The residual gap is deliberate: accesses introduced *after* ABI 6 are not handled
and so are unrestricted. Pinning keeps enforcement identical across kernel upgrades
rather than silently tightening (and breaking builds) or silently loosening.

## seccomp is deny-by-exception, and io_uring is why it exists

An allowlist would have to enumerate every syscall a compiler, linker, test runner
and package manager might make, where one omission breaks a build in a way that is
very hard to attribute. The deny-list targets primitives with no legitimate use from
a build: module loading, kexec, bpf, perf, `ptrace`/`process_vm_*`, clock setting,
`userfaultfd`, and the io_uring family.

io_uring is the load-bearing entry. Its operations are submitted as ring entries
rather than syscalls, so `IORING_OP_CONNECT` and `IORING_OP_OPENAT` never pass
through a filter on `connect`/`openat`. Landlock's LSM hooks *do* cover io_uring for
filesystem access, so file confinement holds either way — but the syscall half is
bypassable unless the ring is refused. Denying `io_uring_setup` closes it
completely: with no ring, no ring operation is reachable, which is why the test
asserts on ring creation rather than on individual operations.

Denials return `EPERM` rather than killing the process, so a denied syscall surfaces
as an ordinary error the command can report instead of a `SIGSYS` death that looks
like a crash in the user's build.

Syscall numbers are hand-maintained via `libc::SYS_*` rather than pulled from
`libseccomp`, avoiding a C dependency; they were taken from this machine's
`asm/unistd_64.h`, not from memory. A name with no known number is a hard error —
an incomplete filter must not be installed silently.

`SOCK_RAW` is matched with a masked comparison, because `SOCK_CLOEXEC` and
`SOCK_NONBLOCK` are OR'd into the same argument and an exact comparison would let
`SOCK_RAW | SOCK_CLOEXEC` straight through.

## The shim verifies its own capabilities

Before applying anything, the shim reads `/proc/self/status` and refuses to run if
`CapEff` or `CapBnd` is non-zero. bwrap should already have emptied both, but if a
future change to the argv stopped doing so, the agent would hold `CAP_NET_ADMIN` in
its user namespace and could rewrite the nftables ruleset that egress policy depends
on. Both sets are checked: an empty effective set alone still leaves a capability
regainable.

## Session namespaces vs per-command namespaces

Split by lifetime, not by convenience:

| Namespace | Lifetime | Why |
|---|---|---|
| user, net, ipc, uts | session | A dev server started by one command must be reachable from the next, which needs a shared network namespace. |
| mount | per command | Rebuilt from the current grant set, so an approved path is simply in the next command's bind list. |
| pid | per command | Killing bwrap then reaps exactly that command's processes and nothing else. |

Commands deliberately do **not** share a PID namespace, contrary to the initial
plan. Sharing it would break the clean per-command reap — `--die-with-parent` only
cascades when bwrap is that namespace's own PID 1 — and would let one command see
and signal another's processes. Measured: two commands in one session reach the same
loopback service while each sees only its own handful of PIDs.

Loopback is **down** in a fresh network namespace, so the session holder brings it
up before signalling readiness. Without that, every connection to `127.0.0.1` inside
the sandbox fails. The holder can do it because it holds `CAP_NET_ADMIN` in its own
user namespace; no agent process ever does.

The namespaces are kept alive by a holder process which blocks reading its stdin, so
if the worker dies the read returns EOF and the session tears itself down — no
orphaned namespaces after a crash. The holder binary is passed in explicitly rather
than taken from `current_exe()`: those differ whenever cowboy is not the running
program, and in an integration test `current_exe()` is the test harness, which gets
launched with an argument it does not understand and fails with no useful message.

Per command, the `setns` calls happen in the forked child via `pre_exec`, never
inline. Doing it on a thread of the worker would move the worker itself into the
sandbox's network namespace and cut it off from the model provider. The user
namespace must be joined first; joining only the network namespace fails with
`EPERM`, because the privilege to do so comes from membership of the owning user
namespace.

## Special filesystems must be mounted before the binds

Originally `--proc`, `--dev` and the tmpfs mounts came *after* the binds, on the
reasoning that then no bind could shadow them. That ordering silently broke runtime
grants: a grant for a path under `/tmp` — a temporary directory, which is exactly
what one reaches for — was shadowed by the `--tmpfs /tmp` applied afterwards. The
grant appeared in `cowboy sandbox plan` and then did not exist inside the sandbox.

The order is now special filesystems first, binds after. Since ordering no longer
prevents shadowing, the plan refuses any bind whose target is, or contains, `/proc`
or `/dev`: a bind over `/proc` would let the agent present a fabricated `/proc` to
its own tooling, and one over `/dev` could hand it a device node of its choosing.
`--remount-ro /` still comes last of all.

## Every running process is stale for a new grant

A Landlock domain is fixed when a process starts and can only ever be narrowed, so
*any* process already running when a grant is approved cannot see it. There is no
such thing as a running process that already has a brand-new grant.

This started out as a generation comparison at grant time, which a test kept
refusing to express as a passing property — because the property was false: the
comparison was always true. The notice now simply names every running process.
Generations survive only where they earn it: `stale_processes()` reports staleness
*afterwards*, so `cowboy proc list` can keep showing it until the process is
restarted, rather than only at the moment of approval.

The behaviour is correct but invisible, and the failure is baffling from the
outside — a dev server that keeps failing to read a folder every new command reads
fine. So it is said out loud, naming the processes and what to do about them.

## The shim's request shares stdin with the command's payload

The shim reads its JSON request from stdin, and the structured file tools also need
to pass a payload on stdin so that multi-line content never has to survive shell
quoting. Both use the same pipe: the request is one newline-terminated line,
followed by the payload.

The shim therefore reads **one byte at a time** up to the newline. A `BufReader`
would read past it into its buffer and silently swallow the start of the payload.
A few hundred single-byte reads is nothing next to the process spawn around it.

## What deleting the control channel removed

The policy engine used to run *inside* the sandbox, in its own container, which
meant it could not be trusted with a decision: every `ask` had to be shipped to the
host over TCP and a verdict shipped back. Because the agent shared the Docker bridge
and could reach that port, the channel needed authenticating. Moving the engine into
the worker deleted the whole apparatus rather than porting it:

- the TCP listener, and the retry loop it needed because the bridge IP did not exist
  until the Docker network came up;
- the eager, detached network-create that existed only to make that retry loop settle
  sooner;
- the per-session token, the `Hello` handshake, and the constant-time comparison
  guarding it (`ct_eq` survives, since `cowboy web` still needs it, and now lives
  next to its one remaining caller);
- the "bind the bridge IP, never `0.0.0.0`" reasoning;
- the `policy-<hash>.json` file written to the runtime dir for the container to read,
  and the care needed to keep it out of a world-writable path;
- the `GatewayMessage`/`HostMessage`/`ControlMessage` wire types.

What replaced it is an `Approver` trait. `DenyAll` makes the fail-closed default
explicit, so a non-interactive run has something truthful to pass rather than being
tempted to skip the check. Because asking is now a function call, the tests can also
assert on *what the user would have been shown* — not just the resulting verdict.

`command_pid` was added to `NetworkAttempt` so a prompt can say which concurrent
command wants a destination. That is a new capability, not a restored one: under
Docker every command shared one uid, so nothing distinguished them either. It is
reported by the relay, which is inside the boundary, so it is a label only and never
feeds an authorization decision.

### Coverage deliberately dropped, to be restored

Three tests were deleted because they exercised machinery that no longer exists:
`gateway_e2e.rs` (allow-listed reachable, un-listed blocked, metadata denied,
non-80/443 dropped), `gateway_approval_e2e.rs` (an approval unblocks an otherwise
denied destination), and `daemon_e2e::e2e_approval_routes_through_worker_and_fails_closed`
(the TCP control channel).

The *properties* they covered are still real and are now only covered at unit level.
The end-to-end versions must be rebuilt against the sandbox transport, and that is
an obligation of the transport work — not something to be quietly lost.

## A successful `connect()` is not evidence of egress

Under transparent interception every `connect()` from the sandbox succeeds, because
it is connecting to the relay on loopback. The real destination has not been
contacted at that point and the policy has not even been consulted. A refusal appears
only when the relay closes the connection, which the client sees as a reset on its
first read.

This bit during Task 7's manual demo: a probe that called `connect()` and printed
success reported `1.1.1.1:443`, `1.1.1.1:9999` and even the black-hole gateway as all
reachable, which looked exactly like a total interception failure. Attempting a
transfer showed all three correctly refused.

Any diagnostic or test that checks reachability must attempt a data transfer.
`tests/sandbox_egress.rs` does, and says so where the helper is defined.

## The relay never creates an outbound socket

For each intercepted connection the relay reports the original destination and, if
allowed, receives an **already-connected descriptor** created by the policy engine in
the host network namespace. A passed descriptor keeps the namespace it was created in
— verified directly: inside a fresh network namespace a process's own `connect()`
fails with `ENETUNREACH`, while a descriptor passed to it over `SCM_RIGHTS` carries a
real HTTP response.

That removes a whole class of bug rather than merely avoiding one. The container
design had to exempt the gateway's own egress from its own redirect rule, by uid, or
the gateway's upstream connections would be redirected back into itself. Here there
is nothing to exempt, so there is no exemption to get wrong — which matters because
in this topology the obvious exemption (`skuid 0`) would have matched the agent.

## The relay channel is the enforcement boundary

Everything else about the sandbox is enforced by the kernel. This channel is
different: the relay *reports* each connection's original destination and the engine
trusts that report. Forging it would defeat every domain rule with namespaces,
Landlock and seccomp all perfectly intact.

It is an **anonymous `socketpair`**, inherited across fork. It has no name in the
filesystem and none in any abstract namespace, so it cannot be opened, connected to,
or enumerated — reaching it requires already holding the descriptor. Backed by three
independent controls: the relay is in a different PID namespace from every agent
command (so it is not even visible in `/proc`), agent commands have an empty
capability bounding set, and `ptrace` is refused by seccomp (with yama
`ptrace_scope=1` restricting it to descendants anyway — and the relay is nobody's
descendant).

Deliberately **not** dependent on `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET`. A named
abstract socket would need that scope to be safe, which would make a Landlock ABI
feature load-bearing for the one boundary that must never degrade. A nameless socket
needs no such rule. There is a test asserting the socket has neither kind of name.

Two further properties, both tested: connecting directly to the relay's port yields
no original destination and is refused rather than proxied (a connection that did not
pass through the nat hook has nothing to honour, and guessing would make the relay an
open proxy); and requests are serialized, because a `SEQPACKET` pair has no request
ids and interleaving two exchanges would let one connection receive another's
descriptor.

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


## DNS is redirected before the loopback bypass

The nat chain's first job used to be `ip daddr 127.0.0.0/8 return`, so that the
agent's own loopback traffic (its dev server, and the relay itself) is not
rewritten. Put DNS after that rule and DNS silently does not work, because a
resolver address is usually *on* loopback: this host's `/etc/resolv.conf` names
the systemd-resolved stub at `127.0.0.53`, and that file is visible inside the
sandbox. Every lookup went to a loopback address with nothing listening.

Port 53 is therefore matched **first**, wherever it is aimed. That has a second
benefit: the agent cannot choose a different resolver by writing its own
`resolv.conf` or by passing `dig @8.8.8.8`. There is a test for exactly that — a
query addressed to `8.8.8.8` comes back REFUSED, which only the host-side resolver
could have produced (the real `8.8.8.8` would answer NXDOMAIN).

The sandbox is never told which resolver is in use. It sends every query to a
loopback port; the address of the real upstream is read from `/etc/resolv.conf` on
the host and never crosses the boundary.

## DNS over TCP dead-ends on purpose

`tcp dport 53` is redirected to the resolver port, where only a UDP socket is
bound, so it is refused. The tempting alternative — letting it fall through to the
catch-all TCP rule — would be a hole: the broker would dial it on the host as an
ordinary TCP connection to a resolver, carrying queries past the record-type gate,
the name gate, and tunnel detection. A refused connection is the correct answer.

This matches the container era, where TCP/53 was also redirected to a UDP-only
resolver. The cost is no large-response fallback; with EDNS0 in play, truncation is
rare.

## The DNS path has its own channel

There are two anonymous socketpairs, not one multiplexed channel. A `SEQPACKET`
pair carries no request ids, so a reply belongs to whoever holds the socket, which
means one exchange at a time. On a single shared channel every connection decision
would queue behind whatever DNS query was waiting on an upstream resolver — seconds,
in the bad case, and `npm install` issues a lot of both.

The alternative was request ids and a demultiplexer, which puts framing logic on
the boundary that decides egress policy. A second nameless socket costs one file
descriptor and no logic, and the trust argument for it is identical to the first.

A related trap: the descriptor numbers the kernel hands out are not ours to choose,
so a source descriptor may *already* be 3 or 4. `dup2`ing them into place naively
can clobber the other channel. Both are staged to high numbers with
`F_DUPFD_CLOEXEC` first.

## The relay is a dumb pipe for DNS

Nothing inside the sandbox parses DNS, gates it, or knows which names are allowed.
The relay reads a datagram off its loopback socket, forwards the bytes, and writes
back whatever bytes come home. Record-type gating, name evaluation, tunnel
detection, transaction-id and question matching, and the `IP → name` map all live on
the host, in `cowboy_gateway::dns::resolve`.

That function is what makes **domain rules enforceable at all**. Before it, only
IP and CIDR rules could match, because nothing recorded which name an address
belonged to. A connection is allowed to an IP because the resolver recorded an
allowed name for it — never because the client said so, since the agent writes the
SNI and the `Host` header itself.

### An upstream failure is a drop, not a refusal

`resolve` returns no bytes when the query is unparseable or the upstream does not
answer, and the relay then sends nothing. Synthesizing REFUSED would be worse than
silence: the client's resolver caches a refusal as an answer, so a transient
upstream blip would look like a policy decision for as long as the cache lives.

### An unknown name resolves

This reads like a hole and is not one. A resolver that parked a query on a human
prompt would simply time out, and resolving is not egress: the connection to
whatever the name resolved to is gated at connect time, where prompting works and a
verdict can be cached per host. Only *denied* names, disallowed record types, and
tunnel-shaped names are refused at the DNS layer — a tunnel's payload **is** the
query, so there is no later connection to gate.

### No resolver configured means no resolution

If `/etc/resolv.conf` names no usable nameserver, the DNS broker refuses every
query and says so once, loudly. It deliberately does not fall back to a public
resolver: silently sending a user's lookups to a third party because their config
could not be read would be a surprising default, and failing is already the safe
direction — names stop resolving, so nothing new becomes reachable.


## Grants are stored outside the workspace

This is the same rule network approvals already followed, and for the same reason.
The workspace is mounted read-write into the sandbox, so a grants file kept in
`.cowboy/` would let a malicious model or a hostile repository widen its own
filesystem access by writing the file. Grants live host-side, under
`~/.config/cowboy/grants/` — a project file keyed by the root's hash, plus one
global file.

The agent therefore cannot grant itself anything. It can only *ask*, with
`request_path`, and the user answers.

### Being host-side is not enough: the denylist runs at use, not at write

Both grant files are hand-editable, and a global grant outlives the project it was
made in — a global grant naming `~/work` is fine until some project keeps
credentials under it. So every grant is re-checked against the credential denylist
when the plan is built, per command, whatever its origin. A refused entry is
dropped and reported (once, since this runs per command) rather than failing the
command, so a stale entry cannot wedge a session.

The user's approval is deliberately **not** the control for credential stores.
The model chooses the path and writes the justification, so a plausible-sounding
request for `~/.ssh` is precisely the attack; `request_path` refuses it after
approval and points at `cowboy secrets add`, which is a deliberate, host-side act.

A test bug worth recording, because it is the failure mode this design is meant to
survive: the first version of the end-to-end test faked `HostProbe::home()` but not
`HostProbe::expand()`, so the denylist expanded `~/.aws` against the *real* home and
the fake credential store was not covered — the sandbox read it, and the test
correctly failed. An implementation where those two disagree silently shrinks the
denylist, so the trait now says they must agree.

## `cowboy grant` needs no channel to the worker

A grant takes effect on the next command of a session that is *already running*,
including one started from a different terminal, and there is no IPC involved. The
sandbox rebuilds its plan per command and re-reads the store as it does, so writing
the file is the whole mechanism.

This is worth naming because the obvious design — a `DaemonReq` variant routed to
the worker, which mutates the live sandbox — is more code, adds a wire message to
the protocol, and only works while a daemon session exists. Reading a small file
per command costs nothing measurable and works in every case, including
`cowboy sandbox exec` and a session that has not started yet.

It also means `cowboy sandbox plan` must include saved grants, or it would describe
a narrower boundary than commands actually run in. That is the one thing that
command must never do.

## Lifetime: there is no "once" for a filesystem grant

`ApprovalScope::Once` maps onto a session-lifetime grant. A grant is a mount and a
Landlock rule established when a command starts, so the smallest unit that can be
expressed is one command — and a grant that evaporated after a single command would
just make the agent ask again mid-task. Mapping it rather than adding a fourth
lifetime keeps one scope type across network and filesystem decisions.

Global scope is not offered in the interactive prompt. Granting a path to *every*
project is a decision to make deliberately at the CLI (`cowboy grant --global`),
not while a task is in flight.

## The grant prompt reuses `ask_user`, not the approval modal

`request_path` asks through `AgentUi::ask_user` with three options, rather than the
`ServerMsg::Approval` modal the network path uses. The modal would have given a
scope picker for free, but it lives on `SocketUi` alone — so the tool would have
worked over the daemon socket and silently denied in the console and TUI paths,
which is the worst kind of difference. `ask_user` is on the trait and implemented by
every UI, so one code path serves all of them with no new wire message.

Anything that is not an explicit approval denies, including the empty string a
non-interactive or unattended session returns. A boundary must not widen because
nobody was watching.
