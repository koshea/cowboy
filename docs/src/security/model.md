# The boundary

The central principle: **the agent is not part of the security boundary**.
Controls are enforced by the Linux kernel and by host-owned configuration the
agent cannot see — never by prompting the model. If the model decides to
misbehave, nothing about the boundary changes.

Cowboy used to get this from Docker. It now gets it from the host directly:
namespaces, Landlock, seccomp, an empty capability set, and a policy engine that
runs in the host process. The reason for the change was flexibility — a container
could not show the agent the toolchain you actually have installed, or let you
approve a folder without a restart — and the boundary came out stronger for it,
because a failure now denies rather than leaks. See
[Sandbox design decisions](sandbox-decisions.md) for the reasoning and the
evidence behind each choice.

There is **no privileged helper**. Every mechanism below works as an ordinary
user; `cowboy` refuses to run with a setuid `bwrap`, and `cowboy doctor` says so.

## Where the agent loop runs

The agent loop runs **host-side**, in the worker process. The sandbox confines the
agent's *shell commands*, not the loop.

That split is deliberate. The loop holds the model credentials and the policy
engine, so it must be somewhere the agent cannot reach; the commands are the
untrusted part. Host-handled tools (memory, artifacts, handoffs, plan, decisions,
scope proposals, path requests) are executed by the loop on the host, so they can
read and write host-visible state — but never the masked host-owned config or the
home-only credentials.

## What the agent can see

Every command runs in a **fresh mount namespace**, built from a plan you can print
at any time:

```console
$ cowboy sandbox plan
```

- **The host toolchain, read-only.** `/usr`, `/opt`, and an *allowlist* of `/etc`
  files. This is the flexibility the image could not offer: the agent gets the
  compilers and runtimes you actually installed, at your versions, with nothing to
  build or pull. `/etc` is an allowlist rather than the whole directory, because
  `/etc` holds shadow, ssh host keys, and every service credential on the box.
- **The project, read-write**, at the workdir (`/workspace` by default).
- **Nothing else.** Your home directory, other projects, and the rest of the
  machine are simply absent — not merely unreadable.
- **The root filesystem is remounted read-only** as the last mount operation.
  Without it the synthetic root is writable, which would put `/etc/ld.so.preload`
  within reach.

Two independent mechanisms enforce this, and they are derived from the same list so
they cannot disagree: the mount namespace decides what *exists*, and **Landlock**
decides what may be opened. Landlock is requested as a hard requirement, not
best-effort — a kernel that cannot provide it is refused rather than quietly
confining less than advertised.

### Host-owned config is masked, not merely unmounted

`security.yaml` and `models.yaml` live under `.cowboy/` inside the project, which
*is* mounted read-write. They are **masked** with an empty read-only file, and the
mask is applied last in the bind order so nothing can re-expose it. Config
validation independently refuses any mount whose source is `security.yaml` or the
`.cowboy` directory.

### Provider credentials are never in the sandbox

Endpoint URLs and API keys live only in `~/.config/cowboy/providers.yaml`
(`0600`), are consumed host-side when building the model client, and are never
written into a project or bound into the sandbox — the agent cannot reach them by
construction. A project `models.yaml` references a provider by name and is
forbidden from carrying credentials.

### Credential stores can never be granted

`~/.aws`, `~/.ssh`, `~/.gnupg`, `~/.password-store`, browser profiles, keyrings,
`~/.netrc`, `~/.npmrc` and the rest are on a **denylist** derived from the same
table `cowboy secrets` uses. They are refused at every scope — including with your
explicit approval, and including a grant already saved to disk.

That last part is the point: the model chooses the path and writes the
justification, so a plausible-sounding request for `~/.ssh` is exactly the attack.
If a task genuinely needs a credential, `cowboy secrets add` is the deliberate,
host-side route.

## Widening the boundary at runtime

A path outside the project reads as "No such file or directory" inside the
sandbox. Two ways to change that, both decided by you:

- the agent calls **`request_path`** with a path and a reason, and you allow it for
  the session, allow and remember it, or deny;
- you run **`cowboy grant <path>`**, which takes effect on the next command —
  including in a session already running.

The agent can only ask. Grants are stored **host-side**, under
`~/.config/cowboy/grants/`, never in the project: the workspace is writable from
inside the sandbox, so a grants file kept there would let a hostile repository
widen its own access.

A grant applies to the *next* command. Anything already running keeps the view it
started with — a Landlock domain is fixed at `exec` and can only ever narrow — so
Cowboy says so out loud rather than leaving you to debug a dev server that cannot
read a folder every new command reads fine.

## What the agent can reach on the network

The session's network namespace holds **no device connected to anything**. That
inverts the usual failure mode: if interception is absent or broken, the agent
reaches *nothing*, rather than reaching everything. Under Docker the namespace had
a real route out and the firewall rules were the only thing in the way.

Traffic is made visible to the policy engine rather than merely blocked, and the
engine runs host-side. See [Network egress](network.md).

## What the agent cannot do

- **No capabilities.** Commands run with an empty *bounding* set, not merely an
  empty effective set, so nothing can be regained. The shim verifies this and
  refuses to exec if it is not true.
- **No `io_uring`.** Denied outright by the seccomp filter, because it is a second
  syscall interface that reaches files and sockets without issuing the syscalls a
  filter watches.
- **No raw sockets, `ptrace`, `bpf`, or module loading.** The filter is
  deny-by-exception with a named list.
- **No `no_new_privs` escape**, and no setuid path back to privilege.
- **No changing its own confinement.** The namespaces, ruleset and Landlock domain
  are established before the first command runs and cannot be widened from inside.

## Resource limits

Memory, CPU and process ceilings are enforced with an unprivileged cgroup v2. They
are **not** part of the security boundary — they protect the machine from a runaway
build, and the sandbox confines correctly without them. `cowboy doctor` warns
rather than fails when they are unavailable, and `cowboy sandbox plan` marks a
configured ceiling this host cannot apply.

## Fail-closed by default

Every gate denies when it cannot get an answer:

- `ask` with no approver attached → **deny**;
- a DNS query whose name is denied → **REFUSED**, without a byte leaving the host;
- a connection whose original destination cannot be recovered → refused, rather
  than forwarded somewhere guessed;
- a sandbox whose confinement cannot be established → the command does not run.
  There is no fallback to running on the host.

## Follow-ups

- Log redaction, per-command secret exposure, secret provenance, and integration
  with 1Password/Vault/SOPS.
- Support for distributions other than the Gentoo target this was built against —
  see [Sandbox design decisions](sandbox-decisions.md) for what is host-specific.
