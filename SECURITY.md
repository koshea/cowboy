# Security policy

Cowboy is a security tool: it runs an AI coding agent inside a host-native sandbox
(namespaces, Landlock and seccomp on Linux; a Seatbelt profile on macOS) while the
**host** enforces the boundary. The threat model and the guarantees are documented
in the [security model](docs/src/security/model.md) and
[network egress](docs/src/security/network.md) chapters. The core principle is that
**the agent is never trusted for security** — controls are enforced by the kernel
sandbox, host-owned config, and the network policy engine, never by prompting the
model.

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

Report privately via GitHub's **"Report a vulnerability"** button under the repo's
**Security** tab (Private Vulnerability Reporting). If that is unavailable, contact
the maintainers privately and we will set up a secure channel.

Please include:

- a description of the issue and its impact (e.g. agent escaping the sandbox,
  reaching host-owned credentials, or bypassing the network policy);
- steps to reproduce, ideally a minimal proof of concept;
- affected version / commit and your environment (OS and kernel version, and on
  Linux your bubblewrap and nftables versions), plus `cowboy doctor` output.

We aim to acknowledge a report within a few days, agree on a disclosure timeline,
and credit reporters who wish to be named once a fix ships.

## Scope

Especially in scope (these are the boundary):

- Reading host-owned config (`security.yaml`, the provider credentials in
  `~/.config/cowboy/providers.yaml`) from inside the sandbox.
- Escaping the sandbox: reading or writing outside the granted paths, regaining
  capabilities, or reaching host unix sockets or the local control sockets.
- Bypassing the network policy (reaching a destination the policy should deny, the
  cloud metadata endpoint, or tunnelling out over DNS).
- A path that lets the agent edit host-owned config, persist a grant, or otherwise
  widen its own boundary.

Out of scope: access the user explicitly granted (`sandbox.mounts`,
`secrets.files`/`secrets.env`, `cowboy grant`, network allow rules), degraded
resource limits that `cowboy doctor` already reports, or attacks that assume an
already-compromised host.

## Supported versions

Cowboy is pre-1.0 and under active development; security fixes target the latest
`main`. Pin a commit if you need stability.
