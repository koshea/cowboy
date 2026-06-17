# Network gateway

Outbound network access is enforced by **routing + dropped capabilities**, not by
asking the model. This is the security thesis.

## Topology (per project)

```
agent  ──(cowboy-int, --internal)──►  gateway(.2) ──(cowboy-egr)──►  internet
        default route forced to .2            applies allow/deny/ask
        NET_ADMIN / NET_RAW dropped           fails closed
```

- The agent container is attached to an **internal-only** Docker network (no
  route to the outside). A separate gateway container is the only path out.
- After the agent starts, a short-lived privileged helper (sharing the agent's
  network namespace) sets the agent's default route to the gateway and
  blackholes the cloud metadata IP. The agent never holds `NET_ADMIN`, so it
  cannot undo this.

## What the gateway enforces (`cowboy-gateway`)

Fail-closed: if the nftables ruleset cannot be applied, the gateway refuses to
run rather than become an open router.

- **nftables**: the `forward` chain DROPs by default (the gateway is not a
  router); only TCP 80/443 from the agent subnet are REDIRECTed to the in-process
  proxy; DNS to the gateway resolver is allowed; everything else outbound is
  dropped.
- **Transparent TLS** (`:8443`): peeks the ClientHello **SNI** (no MITM, no
  decryption), then splices the connection through.
- **Transparent HTTP** (`:8080`): reads the `Host` header.
- **Explicit CONNECT proxy** for proxy-aware clients (convenience).
- **DNS resolver** (`:53`): **policy-enforced** — see below. Records `ip → domain`
  for resolved names so the transparent path can map a destination IP back to a
  hostname for policy.
- **Policy**: deny-list wins, then allow-list (domain via SNI/Host, or CIDR; with
  optional port restriction), else `default_external`. `ask` is sent to the host;
  with no approver it fails closed.

## DNS policy

DNS is a decision point, not a blind relay — otherwise a name lookup
(`<encoded-data>.evil.com`) would be an exfiltration channel even when no
connection to `evil.com` is ever allowed. The resolver:

- **Gates resolution by the policy** (`network_policy.dns.enforce`, default on):
  every query name runs through the same allow/deny/default rules; only names the
  policy **Allows** or you **approve** are forwarded upstream. Denied/unknown names
  are answered **REFUSED locally** and never sent out. (Approving a name covers both
  its resolution and the subsequent connection — one prompt.)
- **Refuses tunnel-prone record types** by default (`TXT`, `NULL`, `ANY`, `AXFR`,
  `IXFR`) — the classic DNS-tunnel/C2 carriers. Opt in per-type via
  `network_policy.dns.allowed_qtypes`.
- **Detects tunneling** (`network_policy.dns.tunnel_detection`, default on): very
  long/high-entropy names, deeply-chunked subdomains, or a high query rate to one
  parent domain are escalated to an **`ask`** prompt (default-deny on timeout), so
  legitimate edge cases aren't silently broken.

Configure under `network_policy.dns` in `.cowboy/security.yaml`. All of it is
fail-closed: a parse failure, disallowed type, denied name, or unreachable approver
yields REFUSED.

## Live approvals

In the TUI, an `ask` opens an approval modal — allow once / session / project /
global, or deny. Project/global approvals persist to `.cowboy/approvals.json` and
merge into the policy on the next run. Non-interactive runs fail closed (deny) and
log the decision.

## Approved Compose/Docker networks

`networks.compose.approved` networks are attached directly to the agent. That
traffic routes peer-to-peer over Docker's own bridge and **bypasses the gateway**
entirely — no prompt. Approve such networks deliberately.

## Honest scope

- Outbound = TCP 80/443 through the gateway with allow/deny/ask by domain/CIDR.
- DNS only via the gateway resolver, **policy-gated** (strict allowlist + tunnel
  detection; risky record types refused).
- Everything else outbound (other TCP ports, all UDP except DNS, ICMP, IPv6) is
  deny-by-default.
- Cloud metadata (`169.254.169.254`) is denied (route blackhole + policy).
- SNI-less / encrypted-ClientHello TLS → ask by IP:port.
- No TLS MITM. DNS is UDP-only (no TCP/53 large-response fallback yet); tunnel
  detection is heuristic (entropy/length/rate), not a guarantee.

Proven end-to-end by the `network_boundary_is_enforced` test: an allow-listed
destination is reachable, an un-listed one is blocked, metadata is denied, and a
non-80/443 port is dropped.
