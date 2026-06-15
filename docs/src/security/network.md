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
- **DNS resolver** (`:53`): forwards upstream and records `ip → domain` so the
  transparent path can map a destination IP back to a hostname for policy.
- **Policy**: deny-list wins, then allow-list (domain via SNI/Host, or CIDR; with
  optional port restriction), else `default_external`. `ask` is sent to the host;
  with no approver it fails closed.

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
- DNS only via the gateway resolver.
- Everything else outbound (other TCP ports, all UDP except DNS, ICMP, IPv6) is
  deny-by-default.
- Cloud metadata (`169.254.169.254`) is denied (route blackhole + policy).
- SNI-less / encrypted-ClientHello TLS → ask by IP:port.
- No TLS MITM. DNS-tunnel detection is out of scope.

Proven end-to-end by the `network_boundary_is_enforced` test: an allow-listed
destination is reachable, an un-listed one is blocked, metadata is denied, and a
non-80/443 port is dropped.
