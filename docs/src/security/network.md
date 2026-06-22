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
  agent's TCP to the in-process proxy and **all** of its DNS (`:53`) to the
  resolver — the DNS redirect runs ahead of Docker's own embedded resolver
  (`127.0.0.11`), so queries can't slip around the gateway. A `filter output` chain
  then **drops by default**, so the residue the REDIRECT can't carry (non-DNS UDP,
  ICMP) can't leak. The gateway's own root-uid egress is exempt so it can reach
  upstream and the host control channel; the agent is kept non-root so it never
  inherits that exemption. Approved Compose subnets bypass the proxy.
- **Transparent proxy** (`:8443`, every port): authorizes a connection by the
  hostname(s) **the gateway itself resolved** for the destination IP — *not* the
  client-presented SNI/Host, which the (untrusted) agent controls. It still sniffs
  the first bytes (TLS SNI / HTTP Host) but only to classify the protocol and to
  flag a name that wasn't among what the gateway resolved (a spoof attempt). No
  MITM, no decryption. A raw-IP connection with no prior lookup falls to CIDR/`ask`
  by `ip:port`.
- **Explicit CONNECT proxy** for proxy-aware clients (the named host is what gets
  dialed, so it's authorized directly).
- **DNS resolver** (`:53`): **policy-enforced** — see below. All agent DNS is
  routed here first (then forwarded to Docker's embedded resolver, preserving
  Compose service discovery), and every answer is recorded `ip → {domains}` so a
  connection's destination IP maps back to the name(s) that resolved to it.
- **Policy**: deny-list wins, then allow-list (domain matched against the
  gateway-resolved name for the IP, or CIDR by the real destination IP; with
  optional port restriction), else `default_external`. A domain allow only grants a
  **public** IP (it can't become a path to an internal address). `ask` is sent to
  the host; with no approver it fails closed.

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
  allow/deny/ask. Connections are attributed by the name the gateway resolved for
  the destination IP (never the agent-supplied SNI/Host), or by CIDR on the real
  IP; a raw IP with no prior lookup → `ask` by `ip:port`.
- DNS only via the gateway resolver, **policy-gated** (strict allowlist + tunnel
  detection; risky record types refused) — including queries the agent aims at
  Docker's embedded resolver.
- Non-DNS UDP, ICMP, and IPv6 are deny-by-default (IPv6 disabled; the rest dropped
  by the `filter output` chain).
- Cloud metadata (`169.254.169.254`) is denied by policy on every port.
- SNI-less / encrypted-ClientHello TLS → ask by IP:port.
- No TLS MITM. DNS is UDP-only (no TCP/53 large-response fallback yet); tunnel
  detection is heuristic (entropy/length/rate), not a guarantee.
- Attribution is by the gateway-resolved `ip → {domains}` map, so it inherits the
  limits of IP-based filtering **without** MITM: a host **co-located on an
  allow-listed CDN IP** is reachable (e.g. another site behind the same Cloudflare
  anycast address as an allowed domain), and an IP-literal with no prior lookup →
  `ask`. Closing the co-hosting gap would require MITM/SNI-pinning.
- Arbitrary **UDP is dropped, not proxied** — proxying it would need TPROXY.
