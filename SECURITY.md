# Security policy

Cowboy is a security tool: it runs an AI coding agent inside a Docker container
while the **host** enforces the boundary. The threat model and the guarantees are
documented in the [security model](docs/src/security/model.md) and
[network gateway](docs/src/security/network.md) chapters. The core principle is
that **the agent is never trusted for security** — controls are enforced by
Docker, host-owned config, and the network gateway, never by prompting the model.

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

Report privately via GitHub's **"Report a vulnerability"** button under the repo's
**Security** tab (Private Vulnerability Reporting). If that is unavailable, contact
the maintainers privately and we will set up a secure channel.

Please include:

- a description of the issue and its impact (e.g. agent escaping the container,
  reaching host-owned credentials, or bypassing the network policy);
- steps to reproduce, ideally a minimal proof of concept;
- affected version / commit and your environment (OS, Docker, nftables versions).

We aim to acknowledge a report within a few days, agree on a disclosure timeline,
and credit reporters who wish to be named once a fix ships.

## Scope

Especially in scope (these are the boundary):

- Reading host-owned config (`security.yaml`, `models.yaml`) or home-only provider
  credentials from inside the container.
- Escaping the container or gaining capabilities the spec drops
  (`NET_ADMIN`/`NET_RAW`).
- Bypassing the egress gateway / network policy (reaching a destination that the
  policy should deny, or the cloud metadata endpoint).
- A path that lets the agent edit host-owned config or otherwise widen its own
  boundary.

Out of scope: issues that require already-granted dangerous options
(`container.privileged`, `container.docker_socket`) — `cowboy doctor` warns about
these by design — or attacks that assume an already-compromised host.

## Supported versions

Cowboy is pre-1.0 and under active development; security fixes target the latest
`main`. Pin a commit if you need stability.
