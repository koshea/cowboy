# Network gateway

Outbound network access is enforced by **in-namespace interception + dropped
capabilities**, not by asking the model. This is the security thesis.

## Topology (per project)

```
agent container ──(cowboy-net)──►  internet
   ▲ shares network namespace
gateway sidecar (--network container:<agent>, NET_ADMIN)
   nft REDIRECT in the shared netns → in-process proxy → allow/deny/ask
   agent has NET_ADMIN / NET_RAW dropped; fails closed
```

- The gateway runs as a **sidecar inside the agent's network namespace**, not as a
  separate routing hop. It installs an nftables `nat output` REDIRECT that captures
  the agent's locally-generated TCP and DNS and hands it to the in-process
  proxy/resolver. The gateway runs as root and exempts its own uid, so its relayed
  (upstream) connections aren't re-intercepted; the agent runs unprivileged with
  `NET_ADMIN`/`NET_RAW` dropped, so it cannot alter the rules.
- Co-locating in the agent's netns means the proxy recovers the original
  destination locally (`SO_ORIGINAL_DST`) and there is **no container-to-router
  forwarding**. That is what lets the same design run on **macOS** (Docker Desktop),
  whose gvisor network backend does not forward traffic through a container acting
  as a router — see [installation](../getting-started/installation.md).

## What the gateway enforces (`cowboy-gateway`)

Fail-closed: if the nftables ruleset cannot be applied, the gateway refuses to
run rather than leave the agent un-sandboxed.

- **nftables**: a `nat output` REDIRECT in the agent's netns sends **all** of the
  agent's TCP to the in-process proxy and DNS (`:53`) to the resolver; a
  `filter output` chain then **drops by default**, so the residue REDIRECT can't
  carry (non-DNS UDP, ICMP) can't leak. The gateway's own root-uid egress is exempt
  so it can reach upstream and the host control channel; approved Compose subnets
  bypass the proxy.
- **Transparent proxy** (`:8443`, every port): sniffs the first bytes — TLS
  **SNI**, else HTTP **Host** — with a short timeout so server-speaks-first
  protocols don't block. No MITM, no decryption. HTTP/HTTPS get hostname precision
  on any port; opaque/encrypted-ClientHello traffic falls back to the DNS map
  (`ip → domain`) or, lacking that, `ask` by `ip:port`.
- **Explicit CONNECT proxy** for proxy-aware clients (convenience).
- **DNS resolver** (`:53`): **policy-enforced** — see below. Records `ip → domain`
  for resolved names so a connection's destination IP maps back to a hostname.
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

- **Every** outbound TCP port is intercepted and gated by domain/CIDR with
  allow/deny/ask; HTTP/HTTPS are attributed by SNI/Host on any port, other
  protocols by the DNS map (`ip → domain`) or `ask` by `ip:port`.
- DNS only via the gateway resolver, **policy-gated** (strict allowlist + tunnel
  detection; risky record types refused).
- Non-DNS UDP, ICMP, and IPv6 are deny-by-default (IPv6 disabled; the rest dropped
  by the `filter output` chain).
- Cloud metadata (`169.254.169.254`) is denied by policy on every port.
- SNI-less / encrypted-ClientHello TLS → ask by IP:port.
- No TLS MITM. DNS is UDP-only (no TCP/53 large-response fallback yet); tunnel
  detection is heuristic (entropy/length/rate), not a guarantee.
- Attribution for non-TLS/HTTP relies on the DNS map, so it inherits its
  limits (shared CDN IPs are coarse; an IP-literal with no prior lookup → `ask`).
- Arbitrary **UDP is dropped, not proxied** — proxying it would need TPROXY.
