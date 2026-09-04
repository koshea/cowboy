# Network egress

Outbound access is enforced by **an empty network namespace plus a host-side policy
engine**, not by asking the model. This is the security thesis, and it is worth
stating in the order that matters:

1. The sandbox's network namespace contains **no device connected to anything**.
2. Interception makes its traffic *visible* to the policy engine.
3. The engine, which runs in the host process, decides and then dials.

Step 1 is containment; steps 2 and 3 are transparency. So a broken or missing
ruleset means the agent reaches **nothing** — not everything. Under the previous
container design the namespace had a real route out and the firewall was the only
thing standing in front of it; that inversion is the single biggest reason the
rewrite was worth doing.

## Topology (per session)

```
   ┌─ sandbox network namespace ──────────────────────┐
   │                                                  │
   │  agent command ──connect()──► nft dnat ──► relay │
   │                                              │   │
   │   black-hole veth (169.254.11.2/24)          │   │
   │   default route → 169.254.11.1 (nothing)     │   │
   └──────────────────────────────────────────────┼───┘
                                                  │ anonymous socketpair
                                                  │ (request; fd back)
   ┌─ host (the worker process) ──────────────────▼───┐
   │  broker → policy engine → connect() in the host  │
   │  namespace → passes the connected fd back        │
   └──────────────────────────────────────────────────┘
```

- A **black-hole veth** exists only so the routing decision succeeds. A
  loopback-only namespace fails `connect()` with `ENETUNREACH` *before* nftables
  sees the packet, so there would be nothing to intercept. Its address is
  link-local — unroutable by definition — and its peer is attached to nothing.
- **nftables** `nat output` rewrites every TCP destination to the relay on
  loopback, and every packet to port 53 to the resolver port. A `filter output`
  chain then drops by default, catching the residue the nat hook cannot carry
  (non-DNS UDP, ICMP) and IPv6 entirely.
- The **relay** runs in the session's namespace and is deliberately *not a proxy*.
- The **broker** and the policy engine run host-side, in the worker.

## The relay never creates an outbound socket

For each intercepted connection the relay reports the original destination
(`SO_ORIGINAL_DST`) to the engine and, if allowed, receives an **already-connected
file descriptor** that the engine created in the *host* network namespace. A passed
descriptor keeps the namespace it was created in, so the relay's traffic never
traverses the sandbox's own routing or firewall rules at all.

This removes a whole class of bug. The container design needed the ruleset to
exempt the gateway's own egress by uid, or its upstream connections would have been
redirected back into itself. That exemption would have been actively dangerous
here, because the agent is uid 0 in its user namespace — `skuid 0` would have
exempted the *agent*. With descriptor passing there is nothing to exempt, so there
is no exemption to get wrong.

Connecting directly to the relay's port is not a way to become a proxy: such a
connection never passed through the nat hook, so it has no original destination and
is refused rather than forwarded somewhere guessed.

## The trust boundary is one socket

Everything else is enforced by the kernel. The relay, though, *reports* a
connection's destination and the engine trusts that report — forge the report and
every domain rule is defeated with all the kernel controls perfectly intact.

So the channel is an **anonymous `socketpair`**, inherited across fork. It has no
name in the filesystem and none in any abstract namespace, so it cannot be opened,
connected to, or enumerated: reaching it requires already holding the descriptor.
Three independent controls back that up, none of which the agent can influence:

- the relay lives in a different PID namespace from every agent command, so no
  agent process can see it, let alone read its `/proc/<pid>/fd`;
- agent commands run with an empty capability bounding set;
- `ptrace` is refused by the seccomp filter, and yama `ptrace_scope=1` would
  restrict it to descendants anyway — and the relay is nobody's descendant.

## What authorizes a connection

The name **the resolver recorded** for the destination IP — never the name the
client presents.

The relay does peek at the first bytes, and forwards them, but only to *classify*
(is this TLS, is it HTTP) and to notice a name that disagrees with what the resolver
saw. The agent writes those bytes, so a request could claim any SNI it liked. There
is no TLS interception and no decryption.

Policy order: deny-list wins; then **approvals** you granted (each scoped to the one
host-or-address and the one port you were asked about); then the allow-list (a domain
matched against the resolved name for that IP, or a CIDR against the real destination
IP, with optional port restriction); otherwise the default for the destination's class.
A domain allow only ever grants a **public** address, so it cannot become a path to an
internal one. `ask` goes to you; with no approver it fails closed.

## DNS

DNS is a decision point, not a blind relay — otherwise a lookup of
`<encoded-data>.evil.com` is an exfiltration channel even when no connection to
`evil.com` is ever allowed. It is also what makes domain rules enforceable at all:
`allow: github.com` works because the resolver records `ip → name` and the
connection is admitted on the strength of that record.

Inside the sandbox, DNS is a **dumb pipe**. The relay reads a datagram off a
loopback socket, forwards the bytes, and writes back whatever comes home. It does
not parse DNS, does not know which names are allowed, and does not even hold the
address of a resolver — so there is no DNS policy inside the boundary to subvert.
Every gate is host-side:

- **Resolution is gated by the policy** (`network_policy.dns.enforce`, default on).
  Denied names are answered **REFUSED locally** and never sent out.
- **Tunnel-prone record types are refused** by default (`TXT`, `NULL`, `ANY`,
  `AXFR`, `IXFR`) — the classic tunnel and C2 carriers. Opt in per type via
  `network_policy.dns.allowed_qtypes`.
- **Tunnel shapes are refused** (`network_policy.dns.tunnel_detection`, default
  on): very long or high-entropy names, deeply chunked subdomains, or a high query
  rate to one parent. This catches `<payload>.allowed.com`, which a name allow-list
  alone cannot. The shape checks first remove any of the host's own **search
  domains** (`/etc/resolv.conf`) from the end of the name: the resolver appends
  those itself, and with `search corp.example` a plain `duckduckgo.com` arrives as
  `duckduckgo.com.corp.example`, whose subdomain region is long and
  high-entropy enough to look like exfiltration. Removing a suffix cannot hide a
  payload — the payload's own labels stay in place and stay scored — and the query
  forwarded upstream is the original bytes either way.
- **Answers are bound to the question that was approved**: the upstream socket is
  `connect()`ed, so the kernel drops datagrams from anyone but the resolver, and a
  reply is accepted only if its transaction id **and** question match what was
  sent. Only a reply that passes both is recorded in the map that authorizes
  connections.

An *unknown* name deliberately **does** resolve. This reads like a hole and is not
one: a resolver that parked a query on a human prompt would simply time out, and
resolving is not egress — the connection to whatever it resolved to is gated at
connect time, where prompting works and a verdict can be cached per host. Only
denied names, disallowed types and tunnel shapes are refused at the DNS layer,
because a tunnel's payload *is* the query and there is no later connection to gate.

Port 53 is intercepted **wherever it is aimed**, so the agent cannot pick its own
resolver by writing a `resolv.conf` or passing `dig @8.8.8.8`. DNS over TCP
dead-ends deliberately: forwarding it as an ordinary connection would carry queries
straight past every gate above.

## Live approvals

An `ask` opens an approval modal in the TUI — allow once / session / project /
global, or deny. Project and global approvals persist host-side (never in the
workspace) and merge into the policy on the next run. Non-interactive runs fail
closed and log the decision. When several commands run at once, the prompt names
the one that is asking.

**An approval grants exactly what the prompt showed**: that host (or that address) on
that port. Not other ports on the same host, not other hosts on that port, and not
subdomains — the prompt named one host, so that is what is granted. If you want a
broader rule, write it in `security.yaml`'s `allow:`, where it is visible and
reviewable; that is also where a domain rule deliberately covers subdomains.

This used not to hold. A persisted approval was decomposed into the policy-wide
`allow` rule set, which is a *cross product* of its domains, CIDRs and ports — so
approving one host on port 22 opened port 22 for every allowed domain, and (with no
`ports:` list configured) approving a host on a non-web port silently did nothing at
all. Approvals are now stored and evaluated as scoped endpoints, matching what the
in-session approval cache already did.

## Honest scope

- **Every** outbound TCP port is intercepted and gated. Attribution is by the
  resolved `ip → {domains}` map or by CIDR on the real IP; a raw IP with no prior
  lookup falls to `ask` by `ip:port`.
- Non-DNS UDP, ICMP and IPv6 are dropped.
- Cloud metadata (`169.254.169.254`) is denied by policy on every port.
- SNI-less or encrypted-ClientHello TLS → `ask` by `ip:port`.
- No TLS MITM. DNS is UDP-only, so there is no large-response TCP fallback; tunnel
  detection is heuristic (entropy, length, rate), not a guarantee.
- IP-based attribution without MITM inherits IP-based limits: a host **co-located
  on an allow-listed CDN address** is reachable — another site behind the same
  Cloudflare anycast IP as an allowed domain, for instance. Closing that gap would
  require MITM or SNI pinning.
- Arbitrary UDP is dropped rather than proxied; proxying it would need TPROXY.
- The `command_pid` shown in a prompt is a **label**, recovered from inside the
  boundary on a best-effort basis. It never authorizes anything.
